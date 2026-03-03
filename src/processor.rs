use crate::buffer::{ChannelMessage, ChannelBuffers, ChannelSnapshot};
use crate::ollama::OllamaClient;
use crate::web::{SharedWebState, WebChannelInfo, WebNewsItem};
use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use chrono_tz::Asia::Jerusalem as TZ_JERUSALEM;
use serde::Deserialize;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::time::{Duration, Instant};
use tokenizers::Tokenizer;
use tokio_util::sync::CancellationToken;

/// Maximum token budget for the LLM prompt (must stay strictly LESS than this).
const MAX_TOKENS: usize = 50_000;

/// The LLM is instructed to focus analysis on this many minutes of recent activity.
/// Context older than this is provided only for background understanding.
const FOCUS_MINUTES: i64 = 15;

/// Don't bother querying the LLM if there are fewer messages than this.
const MIN_MESSAGES_TO_PROCESS: usize = 1;

// ── LLM output types (for JSON parsing validation) ──

#[derive(Deserialize, Debug)]
struct NewsOutput {
    updates: Vec<NewsItem>,
    // Threat assessment fields — the whole point of this alarm system.
    // Non-empty string = active threat (5-word reason why).
    // Empty string = no threat.
    //
    // Group 1: Israel-wide (informational — don't trigger alarm alone)
    #[serde(default)]
    israel_attack_warning: String,
    #[serde(default)]
    israel_actual_red_alerts: String,
    #[serde(default)]
    attack_involves_missiles_not_just_uavs: String,
    //
    // Group 2: Center / Jerusalem (these trigger alarm)
    #[serde(default)]
    jerusalem_attack_warning: String,
    #[serde(default)]
    jerusalem_actual_red_alerts: String,
    #[serde(default)]
    center_dan_or_yehuda_or_jerusalem_danger: String,
    #[serde(default)]
    confirmed_center_attack_not_just_north_south: String,
}

#[derive(Deserialize, Debug)]
struct NewsItem {
    channel: String,
    headline: String,
    importance: String,
    #[serde(default)]
    time_of_report: String,
    #[serde(default)]
    time_of_event: String,
    summary: String,
}

/// Format a Duration into a human-readable string.
/// Examples: "0.3s", "2.3s", "42s", "1m 12s"
fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let millis = d.subsec_millis();

    if total_secs == 0 {
        return format!("0.{}s", millis / 100);
    }
    if total_secs < 10 {
        return format!("{}.{}s", total_secs, millis / 100);
    }
    if total_secs < 60 {
        return format!("{}s", total_secs);
    }
    let minutes = total_secs / 60;
    let secs = total_secs % 60;
    format!("{}m {}s", minutes, secs)
}

/// Maximum number of iteration timestamps to track for rolling average.
const FREQUENCY_WINDOW: usize = 10;

/// The main LLM processing loop. Runs until the shutdown token is cancelled:
///   1. Snapshot all channel buffers
///   2. Binary-search for the optimal time window that maximizes context under 50k tokens
///   3. Build the prompt with uniform per-channel history
///   4. Query the LLM via Ollama
///   5. Parse the JSON response and print a human-readable summary to stdout
///   6. Immediately repeat (no artificial delay — throughput is LLM-bound)
pub async fn run_loop(
    buffers: ChannelBuffers,
    tokenizer: Tokenizer,
    ollama: OllamaClient,
    shutdown: CancellationToken,
    web_state: SharedWebState,
) -> Result<()> {
    let mut consecutive_errors: u32 = 0;
    // Track timestamps of productive iterations for rolling average interval.
    let mut iteration_times: VecDeque<Instant> = VecDeque::with_capacity(FREQUENCY_WINDOW + 1);
    // Track how many messages we last processed so we only re-query when new ones arrive.
    let mut last_processed_total: usize = 0;
    // Track when the LLM became idle (no new messages to process).
    // None = currently working, Some = wall-clock ISO-8601 of when idle started.
    let mut idle_since_wall: Option<String> = None;

    loop {
        if shutdown.is_cancelled() {
            log::info!("[processor] Shutdown signal received — exiting");
            return Ok(());
        }

        // Step 1: Snapshot buffers
        log::info!("[processor] Snapshotting channel buffers...");
        let step_start = Instant::now();
        let snapshot = buffers.snapshot();
        let total_msgs: usize = snapshot.iter().map(|s| s.messages.len()).sum();
        log::info!(
            "[processor] Snapshot complete ({}) — {} channels, {} messages",
            format_duration(step_start.elapsed()),
            snapshot.len(),
            total_msgs
        );

        // Update channel buffer info in web state on every snapshot.
        {
            let mut channels: Vec<WebChannelInfo> = snapshot
                .iter()
                .map(|s| WebChannelInfo {
                    name: s.channel_title.clone(),
                    message_count: s.messages.len(),
                    latest_message: s
                        .messages
                        .last()
                        .map(|m| m.date.to_rfc3339())
                        .unwrap_or_default(),
                })
                .collect();
            channels.sort_by(|a, b| b.latest_message.cmp(&a.latest_message));
            web_state.write().expect("web state lock poisoned").channels = channels;
        }

        // Wait if no messages have arrived yet.
        if snapshot.is_empty() {
            if idle_since_wall.is_none() {
                let now = Utc::now().to_rfc3339();
                idle_since_wall = Some(now.clone());
                web_state.write().expect("web state lock poisoned").idle_since = now;
            }
            log::info!("[processor] No messages yet — waiting 5s...");
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                _ = shutdown.cancelled() => {
                    log::info!("[processor] Shutdown signal received — exiting");
                    return Ok(());
                }
            }
            continue;
        }

        if total_msgs < MIN_MESSAGES_TO_PROCESS {
            if idle_since_wall.is_none() {
                let now = Utc::now().to_rfc3339();
                idle_since_wall = Some(now.clone());
                web_state.write().expect("web state lock poisoned").idle_since = now;
            }
            log::info!(
                "[processor] Only {} messages (need {}) — waiting 5s...",
                total_msgs, MIN_MESSAGES_TO_PROCESS
            );
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                _ = shutdown.cancelled() => {
                    log::info!("[processor] Shutdown signal received — exiting");
                    return Ok(());
                }
            }
            continue;
        }

        // Wait for NEW messages before re-querying the LLM with the same data.
        if total_msgs <= last_processed_total {
            if idle_since_wall.is_none() {
                let now = Utc::now().to_rfc3339();
                idle_since_wall = Some(now.clone());
                web_state.write().expect("web state lock poisoned").idle_since = now;
            }
            log::info!(
                "[processor] No new messages since last query ({} total) — waiting 5s...",
                total_msgs
            );
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                _ = shutdown.cancelled() => {
                    log::info!("[processor] Shutdown signal received — exiting");
                    return Ok(());
                }
            }
            continue;
        }

        // Track iteration frequency (only for productive iterations).
        let now_instant = Instant::now();
        iteration_times.push_back(now_instant);
        if iteration_times.len() > FREQUENCY_WINDOW + 1 {
            iteration_times.pop_front();
        }
        if iteration_times.len() >= 2 {
            let first = *iteration_times.front().unwrap();
            let last = *iteration_times.back().unwrap();
            let total_elapsed = last.duration_since(first);
            let intervals = (iteration_times.len() - 1) as u32;
            let avg = total_elapsed / intervals;
            log::info!(
                "[processor] Average update interval: {} (over last {} runs)",
                format_duration(avg),
                intervals
            );
        }

        // Mark LLM as active (no longer idle).
        if idle_since_wall.is_some() {
            idle_since_wall = None;
            web_state.write().expect("web state lock poisoned").idle_since = String::new();
        }

        // Step 2: Build token-budgeted prompt
        log::info!(
            "[processor] Building token-budgeted prompt ({} channels, {} messages)...",
            snapshot.len(),
            total_msgs
        );
        let step_start = Instant::now();

        match build_budgeted_prompt(&snapshot, &tokenizer) {
            Ok((prompt, window_minutes, token_count)) => {
                log::info!(
                    "[processor] Prompt built ({}): {}min window, ~{} tokens",
                    format_duration(step_start.elapsed()),
                    window_minutes,
                    token_count,
                );

                // Step 3: Query LLM
                log::info!(
                    "[processor] Querying Ollama ('{}')...",
                    ollama.model_name()
                );
                let step_start = Instant::now();

                let query_result = tokio::select! {
                    result = ollama.query(&prompt) => Some(result),
                    _ = shutdown.cancelled() => None,
                };

                match query_result {
                    None => {
                        log::info!("[processor] Shutdown signal received during LLM query — exiting");
                        return Ok(());
                    }
                    Some(Ok(response)) => {
                        log::info!(
                            "[processor] LLM response received ({})",
                            format_duration(step_start.elapsed())
                        );
                        consecutive_errors = 0;
                        last_processed_total = total_msgs;

                        // Step 4: Parse and display output
                        log::info!("[processor] Parsing LLM response...");
                        let step_start = Instant::now();
                        process_and_print_output(&response, &web_state);
                        log::info!(
                            "[processor] Output processed ({})",
                            format_duration(step_start.elapsed())
                        );
                    }
                    Some(Err(e)) => {
                        log::info!(
                            "[processor] LLM query failed after {}",
                            format_duration(step_start.elapsed())
                        );
                        consecutive_errors += 1;
                        let delay_secs = (consecutive_errors as u64 * 5).min(60);
                        log::error!(
                            "Ollama query failed (consecutive error #{}): {:#}. Retrying in {}s...",
                            consecutive_errors,
                            e,
                            delay_secs
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(delay_secs)) => {}
                            _ = shutdown.cancelled() => {
                                log::info!("[processor] Shutdown signal received — exiting");
                                return Ok(());
                            }
                        }
                    }
                }
            }
            Err(e) => {
                log::error!("[processor] Failed to build prompt ({}): {:#}", format_duration(step_start.elapsed()), e);
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                    _ = shutdown.cancelled() => {
                        log::info!("[processor] Shutdown signal received — exiting");
                        return Ok(());
                    }
                }
            }
        }
    }
}

// ── Token counting ──

fn count_tokens(tokenizer: &Tokenizer, text: &str) -> usize {
    tokenizer
        .encode_fast(text, false)
        .map(|enc| enc.len())
        .unwrap_or_else(|e| {
            log::warn!(
                "tokenization failed ({}) — falling back to char-based estimate",
                e
            );
            // Conservative fallback: ~3.5 chars per token for English/mixed text
            text.len() / 3
        })
}

// ── Time window helpers ──

fn find_newest_message_time(snapshots: &[ChannelSnapshot]) -> Option<DateTime<Utc>> {
    snapshots
        .iter()
        .flat_map(|s| s.messages.iter())
        .map(|m| m.date)
        .max()
}

/// Collect messages from each channel that fall within the given time window,
/// providing UNIFORM history depth across all channels (same cutoff time for all).
fn messages_in_window<'a>(
    snapshots: &'a [ChannelSnapshot],
    newest: DateTime<Utc>,
    window: ChronoDuration,
) -> Vec<(&'a ChannelSnapshot, Vec<&'a ChannelMessage>)> {
    let cutoff = newest - window;
    snapshots
        .iter()
        .map(|snap| {
            let msgs: Vec<&ChannelMessage> =
                snap.messages.iter().filter(|m| m.date >= cutoff).collect();
            (snap, msgs)
        })
        .filter(|(_, msgs)| !msgs.is_empty())
        .collect()
}

fn total_message_count(snapshots: &[ChannelSnapshot]) -> usize {
    snapshots.iter().map(|s| s.messages.len()).sum()
}

fn windowed_message_count(
    channel_msgs: &[(&ChannelSnapshot, Vec<&ChannelMessage>)],
) -> usize {
    channel_msgs.iter().map(|(_, msgs)| msgs.len()).sum()
}

// ── Prompt construction ──

fn build_prompt_text(
    channel_messages: &[(&ChannelSnapshot, Vec<&ChannelMessage>)],
    newest: DateTime<Utc>,
    focus_minutes: i64,
) -> String {
    let focus_cutoff = newest - ChronoDuration::minutes(focus_minutes);
    let total_msgs: usize = channel_messages.iter().map(|(_, msgs)| msgs.len()).sum();
    let num_channels = channel_messages.len();

    // Convert UTC to Jerusalem local time for display.
    let newest_local = newest.with_timezone(&TZ_JERUSALEM);
    let cutoff_local = focus_cutoff.with_timezone(&TZ_JERUSALEM);

    // Pre-allocate generously: ~256 bytes per message + prompt boilerplate
    let mut prompt = String::with_capacity(total_msgs * 256 + 4096);

    writeln!(
        &mut prompt,
        "You are a real-time news analyst. Below are the latest messages from \
         {} Telegram news channels ({} messages total). All times are Jerusalem local time (Israel Standard/Daylight Time).",
        num_channels, total_msgs
    )
    .unwrap();
    writeln!(&mut prompt).unwrap();
    writeln!(
        &mut prompt,
        "CURRENT TIME: {} (Jerusalem). This is the timestamp of the most recent message received.",
        newest_local.format("%Y-%m-%d %H:%M:%S"),
    )
    .unwrap();
    writeln!(&mut prompt).unwrap();
    writeln!(
        &mut prompt,
        "YOUR TASK: Analyze ONLY the most recent {} minutes of activity \
         (from {} to {}). Earlier messages are provided for background context only.",
        focus_minutes,
        cutoff_local.format("%H:%M:%S"),
        newest_local.format("%H:%M:%S"),
    )
    .unwrap();
    writeln!(&mut prompt).unwrap();
    writeln!(&mut prompt, "=== CHANNEL MESSAGES ===").unwrap();

    for (snap, msgs) in channel_messages {
        writeln!(&mut prompt).unwrap();
        writeln!(&mut prompt, "--- {} ---", snap.channel_title).unwrap();
        for msg in msgs {
            let local_time = msg.date.with_timezone(&TZ_JERUSALEM);
            writeln!(
                &mut prompt,
                "[{}] {}",
                local_time.format("%Y-%m-%d %H:%M:%S"),
                msg.text
            )
            .unwrap();
        }
    }

    writeln!(&mut prompt).unwrap();
    writeln!(&mut prompt, "=== END MESSAGES ===").unwrap();
    writeln!(&mut prompt).unwrap();

    // The JSON template uses doubled braces {{ }} for literal braces in format strings.
    write!(
        &mut prompt,
        r#"Respond with ONLY a JSON object in the exact format below. No other text before or after the JSON.

{{
  "israel_attack_warning": "",
  "israel_actual_red_alerts": "",
  "attack_involves_missiles_not_just_uavs": "",
  "jerusalem_attack_warning": "",
  "jerusalem_actual_red_alerts": "",
  "center_dan_or_yehuda_or_jerusalem_danger": "",
  "confirmed_center_attack_not_just_north_south": "",
  "updates": [
    {{
      "channel": "channel name",
      "headline": "concise headline (max 15 words)",
      "importance": "critical|high|medium|low",
      "time_of_report": "HH:MM",
      "time_of_event": "HH:MM or unknown",
      "summary": "1-2 sentence information-dense summary"
    }}
  ],
  "meta": {{
    "channels_analyzed": {num_channels},
    "time_window_minutes": {focus_minutes},
    "generated_at": "{now}"
  }}
}}

CRITICAL — THREAT ASSESSMENT FIELDS:
The seven threat fields at the top are the PRIMARY PURPOSE of this system. They are used to wake someone sleeping in Jerusalem to reach a bomb shelter in time. Each field is a STRING, not a boolean:
- When active (threat detected): provide EXACTLY 5 words explaining WHY this is active. This reason is displayed to the user so they immediately understand the situation when woken up.
- When inactive (no threat): use an empty string "".

Set them based on ANY evidence in the messages — even a single credible report is enough. These fields must reflect IMMINENT, ACTIVE threats that warrant waking someone up RIGHT NOW to run to a shelter.

ONCE A DANGER HAS PASSED, ALL FIELDS MUST RETURN TO EMPTY STRING "". If messages describe an attack that already happened and is now over (e.g. "interceptions completed", "all clear given", "sirens ended 10 minutes ago"), set the fields to "". Past tense = "". The fields reflect CURRENT DANGER ONLY, not recent history.

- israel_attack_warning: Non-empty if there is ANY early warning, intelligence, or credible report of an incoming or imminent attack on Israel (missiles, drones, rockets from Iran, Hezbollah, Yemen, etc.). Includes launch detections, military alerts, and "prepare for attack" announcements. Must be CURRENT — not past attacks or theoretical future threats. Example: "Iran launched missiles toward Israel"
- israel_actual_red_alerts: Non-empty if Pikud HaOref (Home Front Command) has activated actual red alert sirens ANYWHERE in Israel, or if credible sources report active sirens/interceptions RIGHT NOW. Example: "Red alert sirens across Israel"
- attack_involves_missiles_not_just_uavs: Non-empty if the CURRENT attack involves missiles, rockets, or ballistic threats — not just UAVs (hostile unmanned aircraft)/drones. UAV attacks on northern or southern Israel are routine and do NOT warrant waking someone in Jerusalem. Only set non-empty when the threat type is missiles, rockets, cruise missiles, or ballistic projectiles. If the attack is UAV-only but targets center Israel, leave this "" (the center-specific fields handle that separately). Example: "Ballistic missiles launched from Iran"
- jerusalem_attack_warning: Non-empty if there is ANY warning or credible report specifically mentioning Jerusalem as a target or at-risk area. Also non-empty if merkaz/center Israel is targeted (missiles to merkaz pass directly over Jerusalem). Example: "Missiles targeting center over Jerusalem"
- jerusalem_actual_red_alerts: Non-empty if red alert sirens are active specifically in Jerusalem RIGHT NOW. Example: "Active sirens sounding in Jerusalem"
- center_dan_or_yehuda_or_jerusalem_danger: Non-empty if any of these areas are at IMMINENT risk: Gush Dan, Yehuda, Jerusalem, Merkaz (center Israel). Even if the target is Merkaz and not explicitly Jerusalem, set this non-empty because the flight path crosses over Jerusalem. Example: "Missiles inbound to Gush Dan"
- confirmed_center_attack_not_just_north_south: THIS FIELD TRIGGERS A LOUD ALARM THAT WAKES A SLEEPING PERSON. It requires CONFIRMED evidence that an attack is actually happening or confirmed launched toward central Israel — not speculation, not "might happen", not political tension. Early warning (10-15 min before impact) is GOOD and desired, but only if the attack is CONFIRMED (e.g. launches detected, missiles in the air, credible military sources confirm an ongoing strike). Set "" if: the danger has passed, the attack is over, only northern or southern areas are affected, or you are guessing/speculating without concrete reports. Set non-empty if: (a) credible sources confirm central Israel / Merkaz / Gush Dan / Tel Aviv / Jerusalem / Yehuda is under active attack or confirmed incoming strike, OR (b) a confirmed attack is underway with broad/unspecified targeting where center cannot be ruled out, OR (c) both north AND center are confirmed targeted simultaneously. Example: "Confirmed strike heading toward center"

IMPORTANT CONTEXT: Pikud HaOref often fails to give Jerusalem explicit early warning. By the time Jerusalem gets an actual siren (~1.5 min warning), Iranian missiles may already be overhead on their way to Merkaz. The user needs ~7 minutes from EARLY WARNING to reach shelter. Therefore: err on the side of a non-empty reason for any field where there is reasonable doubt about an ACTIVE threat. A false positive (unnecessary wake-up) is infinitely better than a false negative (sleeping through an attack). But do NOT set fields non-empty for past events, historical analysis, or speculative future threats — only for RIGHT NOW.

IMPORTANT: An attack ON Israel does NOT mean Israel will attack back imminently. Do not conflate reports of Israeli offensive operations or retaliation plans with incoming threats to Israel. Only set fields non-empty when there is an incoming threat TO Israel, not outgoing attacks FROM Israel.

Rules:
- Respond ENTIRELY in English (the source messages may be in Hebrew, Arabic, or other languages — translate everything to English)
- Include ONLY genuinely notable news updates from the last {focus_minutes} minutes
- If nothing notable happened, return all fields as "" with empty updates array
- Be information-dense and incredibly brief in summaries
- Categorize importance: critical (breaking/urgent), high (significant), medium (noteworthy), low (minor)
- Output ONLY valid JSON, nothing else"#,
        num_channels = num_channels,
        focus_minutes = focus_minutes,
        now = newest_local.to_rfc3339(),
    )
    .unwrap();

    prompt
}

// ── Token-budgeted prompt building (exponential expansion + binary search) ──

/// Build the largest possible prompt that fits under MAX_TOKENS.
///
/// Algorithm:
///   1. Start with a 15-minute window (the focus period).
///   2. Double the window repeatedly (exponential expansion) to include more context.
///   3. When the token count exceeds budget (or all messages are included), stop expanding.
///   4. Binary search between the last-good window and the over-budget window
///      to find the precise optimal fit.
///   5. The time window is uniform across all channels — same cutoff for everyone.
///
/// Returns: (prompt_string, window_in_minutes, token_count)
fn build_budgeted_prompt(
    snapshots: &[ChannelSnapshot],
    tokenizer: &Tokenizer,
) -> Result<(String, u64, usize)> {
    let newest = find_newest_message_time(snapshots)
        .context("no messages found in any channel buffer — nothing to analyze")?;

    let total_all = total_message_count(snapshots);

    // Phase 1: Exponential expansion to find the range boundaries.
    let mut window_minutes: u64 = FOCUS_MINUTES as u64;
    let mut last_good: Option<(String, u64, usize)> = None;

    loop {
        let window = ChronoDuration::minutes(window_minutes as i64);
        let channel_msgs = messages_in_window(snapshots, newest, window);

        if channel_msgs.is_empty() {
            // No messages in this window yet. If we've expanded far enough, give up.
            if window_minutes >= 60 * 24 {
                break;
            }
            window_minutes *= 2;
            continue;
        }

        let prompt = build_prompt_text(&channel_msgs, newest, FOCUS_MINUTES);
        let tokens = count_tokens(tokenizer, &prompt);

        if tokens >= MAX_TOKENS {
            // Over budget — binary search between last_good and current window.
            break;
        }

        // Under budget. Save this as the best known good result.
        last_good = Some((prompt, window_minutes, tokens));

        // Check if we've included every message across all channels.
        let included = windowed_message_count(&channel_msgs);
        if included >= total_all {
            return Ok(last_good.unwrap());
        }

        // Double the window for the next iteration.
        window_minutes = window_minutes.saturating_mul(2);

        // Safety: don't search beyond 7 days (absurdly generous).
        if window_minutes > 60 * 24 * 7 {
            break;
        }
    }

    // Phase 2: Binary search refinement.
    if let Some((good_prompt, good_minutes, good_tokens)) = last_good {
        let high = window_minutes; // This window was over budget (or hit the ceiling).
        match binary_search_window(snapshots, tokenizer, newest, good_minutes, high) {
            Some(refined) => Ok(refined),
            None => Ok((good_prompt, good_minutes, good_tokens)),
        }
    } else {
        // Even the smallest window (FOCUS_MINUTES) exceeded the budget.
        // Binary search between 1 minute and FOCUS_MINUTES to find what fits.
        match binary_search_window(snapshots, tokenizer, newest, 1, FOCUS_MINUTES as u64) {
            Some(result) => Ok(result),
            None => {
                // Even 1 minute exceeds budget. Build it anyway — it's the minimum we can do.
                log::warn!(
                    "even a 1-minute context window exceeds the {} token budget — \
                     channels are extremely active, prompt will be over budget",
                    MAX_TOKENS
                );
                let window = ChronoDuration::minutes(1);
                let channel_msgs = messages_in_window(snapshots, newest, window);
                let prompt = build_prompt_text(&channel_msgs, newest, FOCUS_MINUTES);
                let tokens = count_tokens(tokenizer, &prompt);
                Ok((prompt, 1, tokens))
            }
        }
    }
}

/// Binary search between `low_minutes` and `high_minutes` to find the largest
/// window where the prompt fits under MAX_TOKENS.
fn binary_search_window(
    snapshots: &[ChannelSnapshot],
    tokenizer: &Tokenizer,
    newest: DateTime<Utc>,
    low_minutes: u64,
    high_minutes: u64,
) -> Option<(String, u64, usize)> {
    let mut low = low_minutes;
    let mut high = high_minutes;
    let mut best: Option<(String, u64, usize)> = None;

    // Refine until precision is within 1 minute.
    while high.saturating_sub(low) > 1 {
        let mid = low + (high - low) / 2;
        let window = ChronoDuration::minutes(mid as i64);
        let channel_msgs = messages_in_window(snapshots, newest, window);

        if channel_msgs.is_empty() {
            low = mid;
            continue;
        }

        let prompt = build_prompt_text(&channel_msgs, newest, FOCUS_MINUTES);
        let tokens = count_tokens(tokenizer, &prompt);

        if tokens < MAX_TOKENS {
            best = Some((prompt, mid, tokens));
            low = mid;
        } else {
            high = mid;
        }
    }

    best
}

// ── ANSI color constants ──

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";
const MAGENTA: &str = "\x1b[35m";
const CYAN: &str = "\x1b[36m";
const WHITE: &str = "\x1b[37m";
const BG_RED: &str = "\x1b[41m";
const BRIGHT_RED: &str = "\x1b[91m";
const BRIGHT_GREEN: &str = "\x1b[92m";
const BRIGHT_YELLOW: &str = "\x1b[93m";

/// Format a single threat field as a colored line.
/// Non-empty reason = active threat, empty = no threat.
fn fmt_threat(label: &str, reason: &str) -> String {
    if reason.is_empty() {
        format!("  {DIM}{BRIGHT_GREEN}  no   {RESET}  {DIM}{label}{RESET}")
    } else {
        format!("  {BOLD}{BG_RED}{WHITE}  YES  {RESET}  {BOLD}{BRIGHT_RED}{label}{RESET}  {BOLD}{YELLOW}{reason}{RESET}")
    }
}

/// Return an ANSI color code for the given importance level.
fn importance_color(importance: &str) -> &'static str {
    match importance.to_lowercase().as_str() {
        "critical" => BRIGHT_RED,
        "high" => BRIGHT_YELLOW,
        "medium" => CYAN,
        "low" => DIM,
        _ => WHITE,
    }
}

// ── Output processing ──

/// Parse the LLM response as JSON, print a human-readable summary to stdout,
/// and update the shared web state for the dashboard.
fn process_and_print_output(raw_response: &str, web_state: &SharedWebState) {
    let cleaned = strip_think_blocks(raw_response);
    let json_str = extract_json(&cleaned);

    match serde_json::from_str::<NewsOutput>(json_str) {
        Ok(output) => {
            // any_threat drives ticking sound + red background.
            // Israel-wide fields alone don't trigger it — north/south UAVs aren't
            // worth waking up for. Only center fields OR missiles trigger alarm.
            let any_threat = !output.attack_involves_missiles_not_just_uavs.is_empty()
                || !output.jerusalem_attack_warning.is_empty()
                || !output.jerusalem_actual_red_alerts.is_empty()
                || !output.center_dan_or_yehuda_or_jerusalem_danger.is_empty()
                || !output.confirmed_center_attack_not_just_north_south.is_empty();

            let now = Utc::now().with_timezone(&TZ_JERUSALEM).format("%H:%M:%S");

            // ── Threat assessment panel (always shown) ──
            println!();
            if any_threat {
                println!("{BOLD}{BG_RED}{WHITE}                                                            {RESET}");
                println!("{BOLD}{BG_RED}{WHITE}          !! THREAT ALERT — WAKE UP !!                      {RESET}");
                println!("{BOLD}{BG_RED}{WHITE}                                                            {RESET}");
            } else {
                println!("{DIM}───────────────────────────────────────────────{RESET}");
                println!("  {BOLD}{BRIGHT_GREEN}ALL CLEAR{RESET}  {DIM}[{now}]{RESET}");
                println!("{DIM}───────────────────────────────────────────────{RESET}");
            }
            println!();
            println!("  {BOLD}{MAGENTA}Threat Assessment{RESET}");
            println!("  {DIM}── Israel ──────────────────────────────{RESET}");
            println!("{}", fmt_threat("Attack warning", &output.israel_attack_warning));
            println!("{}", fmt_threat("Red alerts active", &output.israel_actual_red_alerts));
            println!("{}", fmt_threat("Missiles (not just UAVs)", &output.attack_involves_missiles_not_just_uavs));
            println!("  {DIM}── Jerusalem ──────────────────────────{RESET}");
            println!("{}", fmt_threat("Attack warning", &output.jerusalem_attack_warning));
            println!("{}", fmt_threat("Red alerts active", &output.jerusalem_actual_red_alerts));
            println!("  {DIM}── Center / Dan / Yehuda ──────────────{RESET}");
            println!("{}", fmt_threat("Imminent danger", &output.center_dan_or_yehuda_or_jerusalem_danger));
            println!("{}", fmt_threat("Confirmed attack (not just N/S)", &output.confirmed_center_attack_not_just_north_south));
            println!("  {DIM}─────────────────────────────────────────{RESET}");
            println!();

            // ── News items ──
            if output.updates.is_empty() {
                println!("  {DIM}No notable updates in the last {FOCUS_MINUTES} minutes{RESET}");
                println!();
            } else {
                println!("  {BOLD}{BLUE}News Updates{RESET}  {DIM}({} items — last {FOCUS_MINUTES} min){RESET}", output.updates.len());
                println!();
                for item in &output.updates {
                    let color = importance_color(&item.importance);
                    let tag = item.importance.to_uppercase();
                    let time_info = match (item.time_of_report.is_empty(), item.time_of_event.is_empty()) {
                        (false, false) => format!(" {DIM}reported {} / event {}{RESET}", item.time_of_report, item.time_of_event),
                        (false, true) => format!(" {DIM}reported {}{RESET}", item.time_of_report),
                        _ => String::new(),
                    };
                    println!("  {BOLD}{color}[{tag}]{RESET}  {BOLD}{}{RESET}{time_info}", item.headline);
                    println!("         {DIM}{YELLOW}{}{RESET}", item.channel);
                    println!("         {}", item.summary);
                    println!();
                }
            }

            // Update shared web state for the dashboard.
            {
                let mut ws = web_state.write().expect("web state lock poisoned");
                ws.version += 1;
                ws.timestamp = Utc::now().to_rfc3339();
                ws.israel_attack_warning = output.israel_attack_warning.clone();
                ws.israel_actual_red_alerts = output.israel_actual_red_alerts.clone();
                ws.attack_involves_missiles_not_just_uavs = output.attack_involves_missiles_not_just_uavs.clone();
                ws.jerusalem_attack_warning = output.jerusalem_attack_warning.clone();
                ws.jerusalem_actual_red_alerts = output.jerusalem_actual_red_alerts.clone();
                ws.center_dan_or_yehuda_or_jerusalem_danger = output.center_dan_or_yehuda_or_jerusalem_danger.clone();
                ws.confirmed_center_attack_not_just_north_south = output.confirmed_center_attack_not_just_north_south.clone();
                ws.any_threat = any_threat;
                ws.news = output.updates.iter().map(|it| WebNewsItem {
                    channel: it.channel.clone(),
                    headline: it.headline.clone(),
                    importance: it.importance.clone(),
                    time_of_report: it.time_of_report.clone(),
                    time_of_event: it.time_of_event.clone(),
                    summary: it.summary.clone(),
                }).collect();
            }
        }
        Err(e) => {
            log::warn!(
                "LLM response was not valid JSON ({}) — printing raw. \
                 First 500 chars of cleaned output: '{}'",
                e,
                &cleaned[..500.min(cleaned.len())]
            );
            println!(
                "{YELLOW}[{}] (raw){RESET} {}",
                Utc::now().with_timezone(&TZ_JERUSALEM).format("%H:%M:%S"),
                cleaned.trim()
            );
        }
    }
}

/// Strip `<think>...</think>` blocks that Qwen 3.5 may produce before the JSON.
fn strip_think_blocks(text: &str) -> String {
    let mut result = text.to_string();
    // Repeatedly strip think blocks (there should be at most one, but be safe).
    while let Some(start) = result.find("<think>") {
        if let Some(end_tag_start) = result[start..].find("</think>") {
            let end = start + end_tag_start + "</think>".len();
            result = format!("{}{}", &result[..start], &result[end..]);
        } else {
            // Unclosed <think> — strip from <think> to end of string.
            result.truncate(start);
            break;
        }
    }
    result
}

/// Extract the first JSON object from text by finding the outermost `{` and `}`.
/// Handles cases where the LLM includes preamble or postamble around the JSON.
fn extract_json(text: &str) -> &str {
    let trimmed = text.trim();
    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            if end > start {
                return &trimmed[start..=end];
            }
        }
    }
    trimmed
}
