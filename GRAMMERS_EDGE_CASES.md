# grammers Edge Cases, Gotchas, and Known Issues
## For Long-Running Channel Message Listeners

> **Library**: grammers v0.8.x / v0.9.0 (Rust)
> **Research Date**: March 2026
> **Active repo**: https://codeberg.org/Lonami/grammers
> **Archived repo**: https://github.com/Lonami/grammers (read-only since Feb 10, 2026)
> **crates.io downloads**: ~4,709/month

---

## 1. Gap-on-Update Trickle Bug — CRITICAL

**Issue**: [Codeberg #8](https://codeberg.org/Lonami/grammers/issues/8)

When updates arrive in a "trickle" pattern (a new update arrives before the 15-minute gap-resolution timeout expires), the gap resolution mechanism never triggers. Updates queue indefinitely in `possible_gaps` without being delivered to handlers.

### Symptoms
- Console floods with `"gap on update"` log messages for a channel
- Local `pts` freezes while remote `pts` continuously advances
- In high-traffic channels (10K+ messages daily), the issue intensifies
- New messages may still arrive for other channels but gap resolution for the affected channel is blocked

### Root Cause
Per Lonami: *"For as long as you're getting a 'trickle' of updates (a new one before 15 minutes pass), the gap won't be resolved and the updates won't be applied."*

The `message_box` module uses a per-entry deadline that resets each time a related update arrives. If a high-traffic channel continuously sends updates (even unrelated ones), the deadline never expires.

### Status
Identified but Lonami stated: *"There's a fair bit I want to clean up in the message box to get this behavior correct."* May still be active in current versions.

### Mitigation
- Wrap `updates.next()` in `tokio::time::timeout()` with a reasonable deadline (e.g., 5 minutes)
- If timeout triggers without any updates, trigger a manual reconnection (rebuild `SenderPool` + `Client`)
- For high-traffic channels, consider supplementary polling via `client.iter_messages()` to catch missed messages

---

## 2. UpdateStream Permanent Hang After Fatal I/O Error — CRITICAL

**Issue**: [GitHub #388](https://github.com/Lonami/grammers/issues/388)

After fatal network I/O errors (e.g., "read 0 bytes"), the `UpdateStream` enters a permanent `Poll::Pending` state. No waker is set, so `updates.next()` never returns — it deadlocks.

### Symptoms
- Application appears frozen — no updates received, no errors logged
- In single-threaded tokio runtimes, complete deadlock
- Multi-threaded runtimes may not deadlock but the update task hangs forever

### Root Cause
The lower-level `Sender::on_net_read()` detects connection failures, but the higher-level `UpdateStream` does not propagate this error to its `poll_next`. The `ConnectionClosed` variant was later added to `UpdatesLike` to address this.

### Resolution
Partially fixed in later versions via `UpdatesLike::ConnectionClosed` emission from the sender pool (commit `c16dea1`, Dec 26, 2025). Users reported stability after applying the fix.

### Mitigation
**Always wrap `updates.next()` in `tokio::time::timeout()`:**
```rust
use tokio::time::{timeout, Duration};

loop {
    match timeout(Duration::from_secs(300), updates.next()).await {
        Ok(Ok(update)) => handle_update(update),
        Ok(Err(e)) => {
            log::error!("Update stream error: {}", e);
            break; // Rebuild client
        }
        Err(_elapsed) => {
            log::warn!("No updates for 5 minutes — possible hang, reconnecting");
            break; // Rebuild client
        }
    }
}
```

---

## 3. Must Call `iter_dialogs()` at Startup — HIGH

**Issue**: [GitHub #375](https://github.com/Lonami/grammers/issues/375)

Without calling `iter_dialogs()` after login, the session cache lacks channel hashes needed for `getChannelDifference`. Gap recovery fails silently with log messages like `"cannot getChannelDifference for {:?} as we're missing its hash"`.

### Impact
- Updates missed during gaps are never recovered
- You silently lose messages with no error propagated to user code
- A user confirmed the fix: *"I've been testing it for a while, and everything seems to be OK now: gaps are being resolved."*

### Mitigation
**Always call after authentication, before entering the update loop:**
```rust
let mut dialogs = client.iter_dialogs();
while let Some(_dialog) = dialogs.next().await? {
    // Iterating populates the session cache with all peer hashes
}
```

---

## 4. `sync_update_state()` Not Automatic — MEDIUM

**Source**: `grammers-client/src/client/updates.rs:279`

The update state (pts, qts, seq, per-channel pts) is **not automatically persisted** to the SQLite session file on drop. If your process crashes before calling `sync_update_state()`, you will re-process old updates on next startup.

### Mitigation
- Call `sync_update_state()` on graceful shutdown (Ctrl+C handler)
- Call it periodically (e.g., every 60 seconds) to limit re-processing window
- Accept that on hard crash, some updates will be re-delivered (design your handler to be idempotent)

```rust
// Periodic save in a background task
let updates_clone = updates.clone(); // if cloneable, or use a channel
tokio::spawn(async move {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        updates_clone.sync_update_state().await;
    }
});
```

---

## 5. No Built-In Reconnection — MEDIUM

grammers does **not** automatically reconnect when the TCP connection drops.

### What Happens
1. `SenderPoolRunner::run()` at `sender_pool.rs:394` breaks with `Err(ReadError)` when the home sender dies
2. It sends `UpdatesLike::ConnectionClosed` to the updates channel (line 408)
3. The dead connection is removed from the pool (lines 208-209)
4. On the **next API invocation** to that DC, a new connection is created on-demand (lines 237-268)
5. But for `updates.next()`, it may return `Err(InvocationError::Dropped)` when the receiver channel is closed

### Mitigation
Implement an outer reconnection loop:
```rust
loop {
    match run_listener().await {
        Ok(()) => break, // Graceful shutdown
        Err(e) => {
            log::error!("Listener crashed: {}. Reconnecting in 10s...", e);
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }
}
```

Where `run_listener()` creates a fresh `SenderPool`, `Client`, authenticates (session file handles re-auth), calls `iter_dialogs()`, and enters the update loop.

---

## 6. `updateShortMessage` Yields Empty PeerMap — MODERATE

**Issue**: [Codeberg #17](https://codeberg.org/Lonami/grammers/issues/17)

When Telegram sends `updateShortMessage` (a compact format for private chat messages), the library's adapter converts it to `UpdatesCombined` but leaves the `users` and `chats` vectors empty.

### Impact
- `msg.peer()` returns `None`
- `msg.sender()` returns `None`
- `msg.peer_ref()` only works if the peer was previously cached in the session

### For Channel Listeners
**Low impact** — channel messages arrive as `updateNewChannelMessage`, not `updateShortMessage`. This mainly affects private chat messages. But be aware if you expand beyond channels.

### Mitigation
Use `msg.sender_id()` instead of `msg.sender()` — IDs are always available even when the full peer object is not.

---

## 7. Default Update Queue Limit Is 100 — MODERATE

**Source**: `grammers-client/src/client/client.rs:53-105`

`UpdatesConfiguration::update_queue_limit` defaults to `Some(100)`. If your handler processes updates slower than they arrive, excess updates are silently dropped.

### Mitigation
Increase the limit for a channel listener:
```rust
UpdatesConfiguration {
    catch_up: false,
    update_queue_limit: Some(1000), // or None for unlimited
}
```

Or spawn handler work into separate tasks so the update loop itself stays fast:
```rust
Update::NewMessage(msg) if !msg.outgoing() => {
    let buffers = Arc::clone(&shared_buffers);
    tokio::spawn(async move {
        // Process msg...
    });
}
```

---

## 8. `msg_id Too High` Errors on Session Reload — MINOR

**Issue**: [GitHub #389](https://github.com/Lonami/grammers/issues/389)

After saving and reloading a session file, `client.is_authorized()` may trigger repeated `"msg_id too high; re-sending request"` messages. The client generates hundreds of messages before stabilizing (~20 seconds).

### Root Cause
Time offset issues between saved session state and current system clock. The offset should persist after first occurrence but was being recalculated repeatedly.

### Impact
Startup is slow (~20 seconds of churn) but eventually succeeds. No data loss.

### Mitigation
Accept the delay. Ensure NTP is running for accurate system clock.

---

## 9. Race Condition in `generate_random_id` — FIXED

**Issue**: [GitHub #376](https://github.com/Lonami/grammers/issues/376)

Multi-threaded initialization could panic in `generate_random_id` due to an unsafe `compare_exchange().unwrap()` on a static `AtomicI64`. Fixed in commit `91381a1`.

### Impact
None if using recent versions.

---

## 10. `AUTH_KEY_DUPLICATED` — PROTOCOL LEVEL

**Issue**: [GitHub #170](https://github.com/Lonami/grammers/issues/170)

Not a grammers bug but a Telegram server behavior. Occurs with:
- Multiple simultaneous connections using the same auth key
- Network latency (especially with SOCKS5 proxies)
- Session synchronization problems

### Impact
Session is invalidated. Must generate a new auth key and re-login.

### Mitigation
- Never run two instances with the same session file simultaneously
- If using proxies, expect occasional invalidation

---

## 11. Infinite Recursion on Proxy Failures — MODERATE

**Issue**: [GitHub #403](https://github.com/Lonami/grammers/issues/403)

Invalid proxy URL with `SenderPool::with_configuration()` causes a stack overflow. Circular error type conversion: `io::Error` → `ReadError::Io` → `io::Error` → infinite loop.

### Status
Closed as "Not planned" on archived GitHub. Not fixed.

### Mitigation
Validate proxy URLs before passing to grammers. Don't use this feature if you don't need a proxy.

---

## 12. DMs Return `None` as Sender — MINOR

**Issue**: [GitHub #396](https://github.com/Lonami/grammers/issues/396)

When receiving direct messages, `message.sender()` returns `None` because Telegram doesn't always include peer/sender data.

### Mitigation
Use `message.sender_id()` which is guaranteed to work when there is a sender.

---

## 13. Session Management Gotchas

### Session Format Migration (v0.7 → v0.8)
grammers migrated from custom binary `TlSession` to SQLite `SqliteSession` in v0.8.0. Migration code exists but only in the v0.8.0 tag:
```rust
let tl_session = TlSession::load_file("old.session")?;
let session_data = SessionData::from(tl_session);
let sqlite_session = SqliteSession::open("new.session")?;
session_data.import_to(&sqlite_session);
```

### Session Must Be Explicitly Saved
The SQLite session persists auth keys and peer cache automatically (it's a database), but update state (`pts`/`qts`/`seq`) requires calling `sync_update_state()` — see gotcha #4 above.

### Peer Cache Dependency
The session's SQLite database caches every entity encountered. This cache is critical for resolving peers. If empty (fresh session), you cannot interact with channels until you either:
- Call `iter_dialogs()` to populate (recommended)
- Call `resolve_username()` (expensive, triggers flood waits easily — ~200 resolutions/day limit)

---

## 14. Author Warnings

From the grammers README:
> *"The library is in development, but new releases are only cut rarely."*

Recommendation: depend on git version for latest fixes. Also:

> *"As far as I know, this code has not been audited, so if, for any reason, you're using this crate where security is critical, I strongly encourage you to review at least grammers-crypto and the authentication part of grammers-mtproto."*

---

## 15. Alternative: tdlib-rs (If Reliability Is Paramount)

| Factor | grammers | tdlib-rs v1.3.0 |
|--------|----------|-----------------|
| Update gap handling | Partial, known trickle bug | Complete (TDLib handles internally) |
| Reconnection | Manual rebuild required | Automatic |
| Session management | Manual `sync_update_state()` | Automatic |
| Flood wait handling | Auto-sleep below threshold | Automatic |
| Maturity | ~42K total downloads, alpha/beta | Wraps official Telegram C++ library |
| Dependencies | Pure Rust, zero C/C++ | Requires C++ TDLib compilation |
| Binary size | Small | Large |
| Active development | Yes (Codeberg) | Yes (GitHub) |
| API layer | 222 | 1.8.61 |

**tdlib-rs** (v1.3.0, Feb 19, 2026) wraps Telegram's official C++ TDLib via FFI. TDLib handles ALL update complexity internally — gap resolution, reconnection, session management, message ordering. The tradeoff is a C++ build dependency and larger binary.

**Recommendation**: Start with grammers (pure Rust, faster iteration). If you hit reliability issues from the gotchas above, switch to tdlib-rs.

---

## 16. Risk Summary for Long-Running Channel Listener

| Risk | Severity | Likelihood | Mitigation |
|------|----------|-----------|------------|
| Gap-on-update trickle (#8) | HIGH | Medium (high-traffic channels) | `timeout()` + reconnect loop |
| UpdateStream permanent hang (#388) | HIGH | Low (network failures) | Always use `timeout()` |
| Missing channel hashes (#375) | HIGH | Certain (if you skip `iter_dialogs()`) | Call `iter_dialogs()` at startup |
| Update state loss on crash (#4) | MEDIUM | Certain (on any crash) | Periodic `sync_update_state()` |
| No auto-reconnection (#5) | MEDIUM | Medium (network instability) | Outer reconnect loop |
| Update queue overflow (#7) | MEDIUM | Low (fast handlers) | Increase limit or spawn tasks |
| Forced Telegram logout | LOW | Low (non-spammy behavior) | Change cloud password after first login |
| `msg_id` too high on restart (#8) | LOW | Common | Accept ~20s startup delay |
| `AUTH_KEY_DUPLICATED` | LOW | Rare (single instance) | Never run two instances same session |

---

## Sources

- [Codeberg Issue #8: Gap on Update trickle](https://codeberg.org/Lonami/grammers/issues/8)
- [Codeberg Issue #17: updateShortMessage empty PeerMap](https://codeberg.org/Lonami/grammers/issues/17)
- [GitHub Issue #388: UpdateStream permanent hang](https://github.com/Lonami/grammers/issues/388)
- [GitHub Issue #375: Missing channel hashes](https://github.com/Lonami/grammers/issues/375)
- [GitHub Issue #376: Race condition in generate_random_id](https://github.com/Lonami/grammers/issues/376)
- [GitHub Issue #389: msg_id too high](https://github.com/Lonami/grammers/issues/389)
- [GitHub Issue #396: DMs None sender](https://github.com/Lonami/grammers/issues/396)
- [GitHub Issue #403: Infinite recursion on proxy](https://github.com/Lonami/grammers/issues/403)
- [GitHub Issue #170: AUTH_KEY_DUPLICATED](https://github.com/Lonami/grammers/issues/170)
- [GitHub Issue #368: Session format migration](https://github.com/Lonami/grammers/issues/368)
- [GitHub Issue #382: Reconnect policy](https://github.com/Lonami/grammers/issues/382)
- [grammers README](https://codeberg.org/Lonami/grammers)
- [grammers-client docs](https://docs.rs/grammers-client)
- [Telegram Working with Updates](https://core.telegram.org/api/updates)
- [tdlib-rs on GitHub](https://github.com/FedericoBruzzone/tdlib-rs)
- [Real-world grammers projects](https://github.com/Lonami/grammers/wiki/Real-world-projects)
- [tgcli — Telegram CLI using grammers](https://github.com/dgrr/tgcli)
