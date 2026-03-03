mod auth;

use crate::buffer::{ChannelBuffers, ChannelMessage};
use crate::config::Config;
use anyhow::{Context, Result};
use grammers_client::client::UpdatesConfiguration;
use grammers_client::peer::Peer;
use grammers_client::update::Update;
use grammers_client::{Client, SenderPool};
use grammers_session::storages::SqliteSession;
use grammers_session::types::PeerRef;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

/// Errors that should NOT be retried — they indicate a configuration or
/// credentials problem that no amount of reconnecting will fix.
const FATAL_RPC_ERRORS: &[&str] = &[
    "API_ID_INVALID",
    "CONNECTION_API_ID_INVALID",
    "API_ID_PUBLISHED_FLOOD",
    "AUTH_KEY_UNREGISTERED",
    "USER_DEACTIVATED",
    "USER_DEACTIVATED_BAN",
    "SESSION_REVOKED",
    "SESSION_EXPIRED",
];

/// Check if an anyhow error chain contains a fatal Telegram RPC error
/// that should not be retried.
pub fn is_fatal_telegram_error(err: &anyhow::Error) -> bool {
    let error_string = format!("{:#}", err);
    FATAL_RPC_ERRORS
        .iter()
        .any(|fatal| error_string.contains(fatal))
}

/// Run the Telegram listener: connect, authenticate, populate peer cache,
/// then stream channel messages into the shared buffers.
///
/// Returns `Ok(())` on graceful shutdown (Ctrl+C).
/// Returns `Err(...)` on connection/protocol errors (caller should reconnect,
/// UNLESS `is_fatal_telegram_error()` returns true for the error).
///
/// This function is designed to be called in a reconnection loop: on error,
/// the caller waits briefly and calls again. The SQLite session file persists
/// authentication state across reconnections.
pub async fn run_listener(config: &Config, buffers: &ChannelBuffers, ready: Option<Arc<Notify>>) -> Result<()> {
    // 1. Open (or create) the SQLite session file.
    //    This persists: auth keys, peer cache (channel hashes), update state (pts/qts/seq).
    let session = Arc::new(
        SqliteSession::open(&config.session_path)
            .await
            .with_context(|| {
                format!(
                    "failed to open/create Telegram session file at '{}' — \
                     check directory permissions and disk space",
                    config.session_path
                )
            })?,
    );

    // 2. Create the sender pool (manages TCP connections to Telegram DCs).
    //    The runner must be spawned as a background task — it drives I/O.
    let SenderPool {
        runner,
        handle,
        updates,
    } = SenderPool::new(Arc::clone(&session), config.api_id);
    let pool_task = tokio::spawn(runner.run());
    let client = Client::new(handle.clone());

    // 3. Authenticate if needed (interactive: reads code/password from stdin).
    if !client
        .is_authorized()
        .await
        .context("failed to check Telegram authorization status — possible network issue")?
    {
        log::info!("Not authorized with Telegram — starting interactive login");
        auth::interactive_login(&client, &config.phone, &config.api_hash)
            .await
            .context("Telegram interactive authentication failed")?;
        log::info!("Successfully authenticated with Telegram");
    } else {
        log::info!("Already authorized (reusing existing session file)");
    }

    // 4. CRITICAL: Iterate all dialogs to populate the session's peer cache.
    //    Without this, channel hashes are missing and gap recovery fails silently
    //    (see grammers issue #375). We need every channel's hash cached so that
    //    getChannelDifference can resolve gaps.
    log::info!("Loading all dialogs to populate peer cache (required for gap recovery)...");
    let mut dialogs = client.iter_dialogs();
    let mut channel_count: u32 = 0;
    let mut group_count: u32 = 0;
    let mut channels: Vec<(PeerRef, String, i64)> = Vec::new();
    while let Some(dialog) = dialogs
        .next()
        .await
        .context("failed while iterating dialogs for peer cache — possible network issue")?
    {
        match dialog.peer() {
            Peer::Channel(channel) => {
                channel_count += 1;
                let channel_id = channel.id().bare_id().unwrap_or(0);
                channels.push((
                    dialog.peer_ref(),
                    channel.title().to_string(),
                    channel_id,
                ));
            }
            Peer::Group(_) => group_count += 1,
            _ => {}
        }
    }
    log::info!(
        "Peer cache populated: {} broadcast channels, {} groups (monitoring channels only)",
        channel_count,
        group_count
    );

    // 4b. Load historical messages to pre-fill buffers before the LLM processor starts.
    //     This ensures the processor has substantial context from its very first run,
    //     rather than waiting for live messages to trickle in.
    //     Messages arrive newest-first from iter_messages; we reverse each batch so
    //     oldest messages enter the ring buffer first (preserving chronological order).
    log::info!("Loading historical messages (up to 512 per channel)...");
    let mut total_history_msgs: usize = 0;
    for (peer_ref, title, channel_id) in &channels {
        let mut iter = client.iter_messages(*peer_ref).limit(512);
        let mut batch: Vec<ChannelMessage> = Vec::new();
        while let Some(msg) = iter
            .next()
            .await
            .with_context(|| {
                format!(
                    "failed to fetch history for channel '{}' (id: {}) — possible network issue",
                    title, channel_id
                )
            })?
        {
            if msg.outgoing() {
                continue;
            }
            let text = msg.text().to_string();
            if text.is_empty() {
                continue;
            }
            batch.push(ChannelMessage {
                id: msg.id(),
                text,
                date: msg.date(),
                channel_title: title.clone(),
                channel_id: *channel_id,
            });
        }
        // iter_messages returns newest-first; reverse to push oldest-first into the ring buffer.
        batch.reverse();
        let count = batch.len();
        for msg in batch {
            buffers.push(msg);
        }
        if count > 0 {
            log::info!("  {} — {} messages loaded", title, count);
        }
        total_history_msgs += count;
    }
    log::info!(
        "History backfill complete: {} messages across {} channels",
        total_history_msgs,
        channels.len()
    );

    // Signal that authentication, peer cache, and history backfill are complete.
    // The caller uses this to know when it's safe to start the LLM processor
    // (which will now have full buffers from its very first run).
    if let Some(ready) = ready {
        ready.notify_one();
    }

    // 5. Start the update stream.
    //    catch_up: true — recover updates missed during network outages by loading
    //    saved pts/qts from the session file and calling getChannelDifference.
    //    Without this, a 2-minute network drop loses all messages permanently.
    //    update_queue_limit: 1000 — generous buffer (default is 100, too small for busy accounts).
    let mut stream = client
        .stream_updates(
            updates,
            UpdatesConfiguration {
                catch_up: true,
                update_queue_limit: Some(1000),
            },
        )
        .await;

    log::info!(
        "Listening for channel messages (monitoring {} channels, queue limit: 1000)...",
        channel_count
    );

    // 6. Main update loop with:
    //    - Ctrl+C handling for graceful shutdown
    //    - Periodic state saves (every 60s) to persist pts/qts for gap recovery.
    //      Critical: without this, a crash loses all update state and catch_up
    //      has nothing to resume from.
    //    - 5-minute timeout on updates.next() to detect hung connections.
    //      grammers has no TCP keepalive; the MTProto PingDelayDisconnect (75s)
    //      handles server-side detection, but if the client's read() blocks
    //      forever (issue #388), this timeout is the safety net.
    //      For a 2-2.5 minute network outage: the server closes after 75s,
    //      network recovers, client gets RST → error → reconnect. The 5-minute
    //      timeout only fires if something goes truly wrong (permanent hang).
    let mut save_interval = tokio::time::interval(Duration::from_secs(60));
    save_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let result: Result<()> = loop {
        tokio::select! {
            biased;

            _ = tokio::signal::ctrl_c() => {
                log::info!("Ctrl+C received — initiating graceful shutdown");
                break Ok(());
            }

            _ = save_interval.tick() => {
                stream.sync_update_state().await;
                log::debug!(
                    "Periodic update state save (buffering {} channels, {} messages)",
                    buffers.channel_count(),
                    buffers.total_messages()
                );
            }

            result = tokio::time::timeout(Duration::from_secs(300), stream.next()) => {
                match result {
                    Ok(Ok(update)) => {
                        handle_update(update, buffers);
                    }
                    Ok(Err(e)) => {
                        break Err(anyhow::anyhow!(
                            "Telegram update stream returned error: {:#} — \
                             this typically means the connection was lost or the server \
                             rejected our session",
                            e
                        ));
                    }
                    Err(_elapsed) => {
                        break Err(anyhow::anyhow!(
                            "no Telegram updates received for 5 minutes — \
                             possible connection hang (grammers UpdateStream can enter \
                             permanent Poll::Pending after fatal I/O errors). \
                             Triggering reconnection."
                        ));
                    }
                }
            }
        }
    };

    // 7. Cleanup: always save state before disconnecting, regardless of why we're stopping.
    log::info!("Saving Telegram update state before disconnect...");
    stream.sync_update_state().await;
    handle.quit();
    // Wait for the sender pool to shut down cleanly.
    match pool_task.await {
        Ok(()) => log::debug!("Sender pool shut down cleanly"),
        Err(e) => log::warn!("Sender pool task panicked during shutdown: {:?}", e),
    }
    log::info!("Telegram connection closed");

    result
}

/// Process a single Telegram update: if it's a new message from a broadcast
/// channel (not a group, not a DM, not outgoing), push it into the shared buffers.
fn handle_update(update: Update, buffers: &ChannelBuffers) {
    match update {
        Update::NewMessage(msg) if !msg.outgoing() => {
            // msg.peer() returns Option<&Peer>. After iter_dialogs(), this should
            // always be Some for channel messages. We silently skip None (shouldn't happen).
            match msg.peer() {
                Some(Peer::Channel(channel)) => {
                    let text = msg.text().to_string();
                    if text.is_empty() {
                        // Media-only message (photo, video, sticker, etc.) — skip.
                        // We only care about text content for LLM analysis.
                        return;
                    }

                    // bare_id() returns None only for PeerKind::UserSelf, which
                    // cannot occur for a Channel peer. Unwrap is safe here.
                    let channel_id = channel.id().bare_id().unwrap_or_else(|| {
                        log::error!(
                            "BUG: Channel '{}' returned None for bare_id() — \
                             this should never happen for Peer::Channel. Using 0 as fallback.",
                            channel.title()
                        );
                        0
                    });
                    let channel_title = channel.title().to_string();

                    log::debug!(
                        "[{}] {} (ch:{}): {} chars",
                        msg.date().format("%H:%M:%S"),
                        channel_title,
                        channel_id,
                        text.len()
                    );

                    buffers.push(ChannelMessage {
                        id: msg.id(),
                        text,
                        date: msg.date(),
                        channel_title,
                        channel_id,
                    });
                }
                Some(Peer::Group(_)) => {
                    // Supergroup/megagroup — skip per requirements (channels only).
                }
                Some(Peer::User(_)) => {
                    // Private message — skip.
                }
                None => {
                    // Peer not in cache. Shouldn't happen after iter_dialogs(),
                    // but can occur for updateShortMessage edge case.
                    log::trace!("received message with unknown peer (msg_id: {})", msg.id());
                }
            }
        }
        // MessageEdited, MessageDeleted, CallbackQuery, etc. — not relevant for news monitoring.
        _ => {}
    }
}
