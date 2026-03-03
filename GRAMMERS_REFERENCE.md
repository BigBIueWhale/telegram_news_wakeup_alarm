# grammers Rust Library — API Reference

> **grammers** v0.8.x (git HEAD as of March 2026) — Telegram Layer 222
> **Author**: Lonami (same as Python's Telethon)
> **License**: MIT OR Apache-2.0
> **Active repo**: https://codeberg.org/Lonami/grammers (GitHub archived Feb 10, 2026)
> **Rust edition**: 2024

---

## 1. Crate Versions (Pinned)

| Crate | Version | Purpose |
|-------|---------|---------|
| `grammers-client` | **0.8.1** | High-level client API |
| `grammers-session` | **0.8.0** | SQLite session storage |
| `grammers-mtsender` | **0.8.1** | Network sender pool |
| `grammers-mtproto` | **0.8.0** | MTProto protocol implementation |
| `grammers-tl-types` | **0.8.0** | Auto-generated Telegram type definitions |
| `grammers-crypto` | **0.8.0** | Cryptographic primitives |
| `grammers-tl-gen` | **0.8.0** | TL code generator |
| `grammers-tl-parser` | **1.2.0** | TL schema parser |

**Important**: These are git HEAD versions. crates.io published versions may lag. The author recommends depending on git for latest fixes:
```toml
grammers-client = { git = "https://codeberg.org/Lonami/grammers.git" }
```

### Feature Flags (`grammers-client`)

| Feature | Enables | Default |
|---------|---------|---------|
| `fs` | File I/O for downloads via `tokio/fs` | **Yes** |
| `markdown` | `message.markdown_text()` | No |
| `html` | `message.html_text()` | No |
| `proxy` | SOCKS4/SOCKS5/HTTP CONNECT proxy | No |
| `parse_invite_link` | `Client::parse_invite_link()` | No |

### Feature Flags (`grammers-session`)

| Feature | Enables | Default |
|---------|---------|---------|
| `sqlite-storage` | `SqliteSession` (backed by libsql) | **Yes** |
| `serde` | Serde serialization of session types | No |

---

## 2. Client Setup (Three-Step Architecture)

### Step 1: Open Session

```rust
use grammers_session::storages::SqliteSession;
use std::sync::Arc;

let session = Arc::new(SqliteSession::open("telegram.session").await?);
```

Source: `grammers-session/src/storages/sqlite.rs:172`

The SQLite session stores: home datacenter ID, DC options (IP, port, auth keys as 256-byte blobs), peer info cache (peer_id, hash, subtype), update state (pts, qts, date, seq), per-channel state (peer_id, pts).

### Step 2: Create Sender Pool

```rust
use grammers_mtsender::SenderPool;

let SenderPool { runner, handle, updates } = SenderPool::new(Arc::clone(&session), api_id);
let pool_task = tokio::spawn(runner.run());
```

Source: `grammers-mtsender/src/sender_pool.rs:151-191`

Returns three things:
- **`runner: SenderPoolRunner`** — must be spawned as a tokio task
- **`handle: SenderPoolFatHandle`** — cheaply cloneable, passed to Client
- **`updates: mpsc::UnboundedReceiver<UpdatesLike>`** — the single channel for receiving updates

### Step 3: Create Client

```rust
use grammers_client::Client;

let client = Client::new(handle.clone());
```

Source: `grammers-client/src/client/net.rs:53-54`

`Client` is cheaply cloneable (`#[derive(Clone)]` wrapping `Arc<ClientInner>`).

---

## 3. Authentication Flow

### Check If Already Authorized

```rust
if client.is_authorized().await? {
    // Session file has valid auth key — skip login
}
```

Source: `grammers-client/src/client/auth.rs:103`

### User Account Login (Phone + Code + Optional 2FA)

```rust
use grammers_client::SignInError;

// Step 1: Request code
let token = client.request_login_code(&phone, &api_hash).await?;

// Step 2: Sign in with code
match client.sign_in(&token, &code).await {
    Ok(user) => println!("Logged in as {}", user.first_name()),

    Err(SignInError::PasswordRequired(password_token)) => {
        // Step 3: 2FA password required
        let hint = password_token.hint().unwrap_or("None");
        println!("2FA hint: {}", hint);
        client.check_password(password_token, password.trim()).await?;
    }

    Err(SignInError::InvalidCode) => bail!("Invalid verification code"),
    Err(SignInError::SignUpRequired) => bail!("Phone not registered with Telegram"),
    Err(SignInError::InvalidPassword(_)) => bail!("Wrong 2FA password"),
    Err(SignInError::Other(e)) => return Err(e.into()),
}
```

Source: `grammers-client/src/client/auth.rs:242-416`

### SignInError Enum

```rust
pub enum SignInError {
    SignUpRequired,
    PasswordRequired(PasswordToken),
    InvalidCode,
    InvalidPassword(PasswordToken),
    Other(InvocationError),
}
```

Source: `grammers-client/src/client/auth.rs:23`

### Bot Login (Simpler)

```rust
client.bot_sign_in(&bot_token, &api_hash).await?;
```

Source: `grammers-client/src/client/auth.rs:183`

---

## 4. Subscribing to Updates

### Create Update Stream

```rust
use grammers_client::client::UpdatesConfiguration;

let mut updates = client.stream_updates(updates, UpdatesConfiguration {
    catch_up: false,              // true = fetch updates missed while offline
    update_queue_limit: Some(100), // None = unlimited (risky), Some(0) = drop all
}).await;
```

Source: `grammers-client/src/client/updates.rs:298-323`

### Main Update Loop

```rust
loop {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => break,
        result = updates.next() => {
            let update = result?;
            match update {
                Update::NewMessage(message) if !message.outgoing() => {
                    // Handle incoming message
                }
                _ => {}
            }
        }
    }
}
```

Source: `grammers-client/src/client/updates.rs:94`

### The Update Enum

```rust
#[non_exhaustive]  // MUST always have a catch-all _ branch
#[derive(Debug, Clone)]
pub enum Update {
    NewMessage(Message),
    MessageEdited(Message),
    MessageDeleted(MessageDeletion),
    CallbackQuery(CallbackQuery),
    InlineQuery(InlineQuery),
    InlineSend(InlineSend),
    Raw(Raw),
}
```

Source: `grammers-client/src/update/update.rs:21-44`

**Critical**: Both `tl::enums::Update::NewMessage` AND `tl::enums::Update::NewChannelMessage` are mapped to `Update::NewMessage`. You must check the peer type to determine the source.

### Save State Before Exit

```rust
updates.sync_update_state().await;
handle.quit();
let _ = pool_task.await;
```

**`sync_update_state()` is NOT automatic on drop.** If you don't call it, update state is lost and you'll re-process old updates on restart.

Source: `grammers-client/src/client/updates.rs:279`

---

## 5. Message Type — Key Methods

The `update::Message` implements `Deref` to `message::Message`, so all methods below are available directly.

| Method | Signature | Returns | Source Line |
|--------|-----------|---------|-------------|
| `text()` | `pub fn text(&self) -> &str` | Message text (empty for media-only) | `message.rs:341` |
| `id()` | `pub fn id(&self) -> i32` | Message ID | `message.rs:233` |
| `date()` | `pub fn date(&self) -> DateTime<Utc>` | UTC timestamp | `message.rs:332` |
| `peer_id()` | `pub fn peer_id(&self) -> PeerId` | Chat/channel/user ID | `message.rs:238` |
| `peer()` | `pub fn peer(&self) -> Option<&Peer>` | Full peer object | `message.rs:257` |
| `sender()` | `pub fn sender(&self) -> Option<&Peer>` | Message sender | `message.rs:290` |
| `sender_id()` | `pub fn sender_id(&self) -> Option<PeerId>` | Sender ID (more reliable than `sender()`) | `message.rs:262` |
| `outgoing()` | `pub fn outgoing(&self) -> bool` | Sent by us? | `message.rs:137` |
| `post()` | `pub fn post(&self) -> bool` | Is channel post? | `message.rs:179` |
| `post_author()` | `pub fn post_author(&self) -> Option<&str>` | Author signature | `message.rs:531` |
| `silent()` | `pub fn silent(&self) -> bool` | Silent message? | `message.rs:169` |
| `pinned()` | `pub fn pinned(&self) -> bool` | Pinned message? | `message.rs:207` |
| `media()` | `pub fn media(&self) -> Option<Media>` | Media attachment | `message.rs:394` |
| `photo()` | `pub fn photo(&self) -> Option<Photo>` | Photo if present | `message.rs:788` |
| `forward_header()` | `pub fn forward_header(&self) -> Option<...>` | Forward info | `message.rs:296` |
| `reply_to_message_id()` | `pub fn reply_to_message_id(&self) -> Option<i32>` | Reply target | `message.rs:574` |
| `markdown_text()` | `pub fn markdown_text(&self) -> String` | (feature `markdown`) | `message.rs:367` |
| `html_text()` | `pub fn html_text(&self) -> String` | (feature `html`) | `message.rs:382` |

The `update::Message` struct also exposes:
- `pub raw: tl::enums::Update` — raw TL update (can check `NewChannelMessage` vs `NewMessage`)
- `pub state: State` — update state at time of delivery

Source: `grammers-client/src/update/message.rs:19-37`

---

## 6. Filtering for Channel Messages Only

grammers has **no built-in filter system**. You pattern-match manually.

### Strategy: Check Peer Type

```rust
use grammers_client::update::Update;
use grammers_client::peer::Peer;

match update {
    Update::NewMessage(message) if !message.outgoing() => {
        match message.peer() {
            Some(Peer::Channel(channel)) => {
                // BROADCAST CHANNEL — this is what we want
                let title: &str = channel.title();
                let username: Option<&str> = channel.username();
                let text: &str = message.text();
                let date: DateTime<Utc> = message.date();
                let msg_id: i32 = message.id();
                // Process...
            }
            Some(Peer::Group(_)) => {
                // Megagroup/supergroup — SKIP for our use case
            }
            Some(Peer::User(_)) => {
                // Private message — SKIP
            }
            None => {
                // Peer not in cache — SKIP (shouldn't happen after iter_dialogs)
            }
        }
    }
    _ => {} // Required due to #[non_exhaustive]
}
```

### Peer Types

| Peer variant | What it is | `peer.name()` returns |
|-------------|-----------|----------------------|
| `Peer::Channel(channel)` | Broadcast channel (`broadcast == true`) | `Some(title)` |
| `Peer::Group(group)` | Megagroup or basic group | `Some(title)` |
| `Peer::User(user)` | Private chat | `Some(first_name)` |

Source: `grammers-client/src/peer/mod.rs:66-85`

### Channel-Specific Methods

```rust
channel.title()     // -> &str (never None)
channel.username()  // -> Option<&str>
channel.id()        // -> PeerId
```

Source: `grammers-client/src/peer/channel.rs:150`

### Alternative: Check Raw Update Type

```rust
match &update_message.raw {
    tl::enums::Update::NewChannelMessage(_) => { /* from channel/megagroup */ }
    tl::enums::Update::NewMessage(_) => { /* from user/small group */ }
    _ => {}
}
```

---

## 7. Getting All Joined Channels (iter_dialogs)

**You MUST call this at startup.** Without it, channel hashes are missing and gap recovery fails silently.

```rust
let mut dialogs = client.iter_dialogs();
let total = dialogs.total().await?;
println!("Total dialogs: {}", total);

while let Some(dialog) = dialogs.next().await? {
    match dialog.peer() {
        Peer::Channel(channel) => {
            println!("Channel: {} (ID: {})", channel.title(), channel.id());
        }
        Peer::Group(group) => {
            println!("Group: {:?}", group.title());
        }
        Peer::User(user) => {
            println!("User: {:?}", user.first_name());
        }
    }
}
```

Source: `grammers-client/src/client/dialogs.rs:133-155`

The `stream_updates` docs state explicitly:
> **Important** to note that for gaps to be resolved, the peers must have been persisted in the session cache beforehand (i.e. be retrievable with `Session::peer`). A good way to achieve this is to use `Self::iter_dialogs` at least once after login.

The `Dialog` struct:
```rust
pub struct Dialog {
    pub raw: tl::enums::Dialog,
    pub peer: Peer,
    pub last_message: Option<Message>,
}
```

Source: `grammers-client/src/peer/dialog.rs`

---

## 8. Rate Limiting / Flood Wait

### AutoSleep (Default Retry Policy)

```rust
pub struct AutoSleep {
    pub threshold: Duration,           // Default: 60 seconds
    pub io_errors_as_flood_of: Option<Duration>,  // Default: Some(1 second)
}
```

Source: `grammers-client/src/client/retry_policy.rs:50-94`

- Flood wait ≤ 60s: library **automatically sleeps** and retries (once)
- Flood wait > 60s: error is propagated to caller
- I/O errors treated as 1-second floods (retry once)

### RPC Error Structure

```rust
pub struct RpcError {
    pub code: i32,           // 420 for flood wait
    pub name: String,        // "FLOOD_WAIT"
    pub value: Option<u32>,  // seconds to wait
    pub caused_by: Option<u32>,
}
```

Source: `grammers-mtsender/src/errors.rs:79-108`

### Custom Retry Policy

Implement the `RetryPolicy` trait:
```rust
pub trait RetryPolicy: Send + Sync {
    fn should_retry(&self, ctx: &RetryContext) -> ControlFlow<(), Duration>;
}
```

Or use `NoRetries` to disable all automatic retries.

---

## 9. InvocationError Enum

```rust
pub enum InvocationError {
    Rpc(RpcError),
    Io(io::Error),
    Deserialize(mtp::DeserializeError),
    Transport(transport::Error),
    Dropped,        // Connection lost
    InvalidDc,
    Authentication(authentication::Error),
}
```

Source: `grammers-mtsender/src/errors.rs:196-233`

Key method: `pub fn is(&self, rpc_error: &str) -> bool` — supports wildcards like `"FLOOD_WAIT"`.

---

## 10. Keep-Alive and Ping

Built-in automatic pings:
- **Ping interval**: 60 seconds (`PING_DELAY`)
- **Disconnect delay**: 75 seconds (`NO_PING_DISCONNECT`)
- Uses `PingDelayDisconnect` RPC

Source: `grammers-mtsender/src/sender.rs:49-58`

---

## 11. Proxy Support

Enable with `features = ["proxy"]`:
```toml
grammers-client = { version = "0.8.1", features = ["proxy"] }
```

Supports SOCKS4, SOCKS5, HTTP CONNECT. Configured via `ConnectionParams::proxy_url`.

IPv6 support added in commit `85dc937` (Feb 11, 2026) via `ConnectionParams::use_ipv6`.

---

## 12. Resolve Username

```rust
let peer: Option<Peer> = client.resolve_username("channel_username").await?;
```

Source: `grammers-client/src/client/chats.rs:381`

**Warning**: Rate-limited heavily (~200 resolutions/day). Prefer `iter_dialogs()` for bulk discovery.

---

## 13. Recent Git Activity (Stability Indicators)

| Date | Commit | Description |
|------|--------|-------------|
| 2026-02-28 | (Codeberg) | Load MessageBox state for sessions |
| 2026-02-11 | `85dc937` | Add `use_ipv6` flag |
| 2026-02-10 | `fa7692e` | Migrate to Codeberg, archive GitHub |
| 2026-02-10 | `8de15a3` | Add SOCKS4 and HTTP CONNECT proxy |
| 2026-02-09 | `4733792` | Add `requires_join_request` methods |
| 2026-02-07 | `342bed0` | Update to Telegram layer 222 |
| 2026-01-29 | `686c8cd` | Disable unused libsql features |
| 2026-01-17 | `9551ce6` | Add subscription status and fake flag |
| 2026-01-17 | `038f676` | Introduce `SenderPoolFatHandle` |
| 2026-01-15 | `525dee0` | Switch to libsql for sessions |
| 2026-01-12 | `c432d59` | Make `Session` trait async |
| 2026-01-05 | `177c3fe` | Update to layer 221 |
| 2025-12-26 | `c16dea1` | Bring back `ConnectionClosed` notification |
| 2025-12-26 | `8c225e2` | Support I/O errors as flood on `AutoSleep` |
| 2025-12-26 | `7c9c842` | Bring back `RetryPolicy` trait |

The project is **very actively maintained** with significant architectural improvements in late 2025 / early 2026.

---

## 14. Type Quick Reference

```
Update::NewMessage(update::Message)
  |-- Deref --> message::Message
  |     |-- .text() -> &str
  |     |-- .id() -> i32
  |     |-- .peer_id() -> PeerId
  |     |-- .peer() -> Option<&Peer>
  |     |        |-- Peer::User(User)
  |     |        |-- Peer::Group(Group)
  |     |        |-- Peer::Channel(Channel)
  |     |              |-- .title() -> &str
  |     |              |-- .username() -> Option<&str>
  |     |              |-- .id() -> PeerId
  |     |-- .sender() -> Option<&Peer>
  |     |-- .sender_id() -> Option<PeerId>
  |     |-- .outgoing() -> bool
  |     |-- .post() -> bool
  |     |-- .date() -> DateTime<Utc>
  |     |-- .media() -> Option<Media>
  |-- .raw: tl::enums::Update
  |-- .state: State
```

---

## 15. Complete Example: Channel Listener Skeleton

```rust
use anyhow::{bail, Context, Result};
use grammers_client::client::UpdatesConfiguration;
use grammers_client::peer::Peer;
use grammers_client::update::Update;
use grammers_client::Client;
use grammers_mtsender::SenderPool;
use grammers_session::storages::SqliteSession;
use std::sync::Arc;

const SESSION_FILE: &str = "telegram.session";

#[tokio::main]
async fn main() -> Result<()> {
    simple_logger::init_with_level(log::Level::Warn)?;

    let api_id: i32 = std::env::var("TELEGRAM_API_ID")?.parse()?;
    let api_hash: String = std::env::var("TELEGRAM_API_HASH")?;

    // 1. Open session
    let session = Arc::new(
        SqliteSession::open(SESSION_FILE)
            .await
            .context("failed to open session file")?,
    );

    // 2. Create sender pool
    let SenderPool { runner, handle, updates } =
        SenderPool::new(Arc::clone(&session), api_id);
    let pool_task = tokio::spawn(runner.run());
    let client = Client::new(handle.clone());

    // 3. Authenticate
    if !client.is_authorized().await? {
        let phone = std::env::var("TELEGRAM_PHONE")?;
        let token = client.request_login_code(&phone, &api_hash).await?;
        println!("Enter the code Telegram sent:");
        let mut code = String::new();
        std::io::stdin().read_line(&mut code)?;
        match client.sign_in(&token, code.trim()).await {
            Ok(_) => println!("Logged in!"),
            Err(grammers_client::SignInError::PasswordRequired(pw_token)) => {
                println!("2FA password (hint: {:?}):", pw_token.hint());
                let mut pw = String::new();
                std::io::stdin().read_line(&mut pw)?;
                client.check_password(pw_token, pw.trim()).await?;
            }
            Err(e) => bail!("sign-in failed: {:?}", e),
        }
    }

    // 4. Populate peer cache (CRITICAL)
    let mut dialogs = client.iter_dialogs();
    let mut channel_count = 0;
    while let Some(dialog) = dialogs.next().await? {
        if matches!(dialog.peer(), Peer::Channel(_)) {
            channel_count += 1;
        }
    }
    println!("Cached {} channels", channel_count);

    // 5. Stream updates
    let mut stream = client
        .stream_updates(updates, UpdatesConfiguration {
            catch_up: false,
            update_queue_limit: Some(500),
        })
        .await;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("Shutting down...");
                break;
            }
            result = stream.next() => {
                let update = result.context("update stream error")?;
                match update {
                    Update::NewMessage(msg) if !msg.outgoing() => {
                        if let Some(Peer::Channel(ch)) = msg.peer() {
                            println!("[{}] {}: {}",
                                msg.date().format("%H:%M:%S"),
                                ch.title(),
                                msg.text(),
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // 6. Save state
    stream.sync_update_state().await;
    handle.quit();
    let _ = pool_task.await;

    Ok(())
}
```
