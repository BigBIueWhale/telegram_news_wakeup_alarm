# Telegram MTProto Protocol Reference

> For building a Rust channel message listener using grammers.
> Focus: update system, gap recovery, channels, ToS, rate limits.

---

## 1. The Update System — pts, qts, seq, date

Telegram uses four sequencing fields to maintain consistency between client and server.

### `seq` (Global Sequence Number)

A global sequence number for the overall updates stream. Arrives with `updates` and `updatesCombined` constructors.

**Ordering rules:**
- `seq_start === 0`: apply immediately (unordered special case)
- `local_seq + 1 === seq_start`: next expected — safe to apply
- `local_seq + 1 > seq_start`: already processed — ignore (duplicate)
- `local_seq + 1 < seq_start`: **gap detected** — must recover

### `pts` (Persistent Timestamp / Message Box Sequence)

Each event in a "message box" (message created, edited, deleted) gets a unique autoincrementing `pts`.

**Validation formula:**
- `local_pts + pts_count === pts`: safe to apply
- `local_pts + pts_count > pts`: duplicate — discard
- `local_pts + pts_count < pts`: **gap** — must recover

**Example**: With `local_pts = 131`, receiving `updateNewChannelMessage` with `pts = 132` and `pts_count = 1`: `131 + 1 = 132` ✓

### `qts` (Secondary Sequence)

Used for secret chats and certain bot events. Works identically to `pts` but `qts_count` is always 1.

### `date` (Unix Timestamp)

A Unix timestamp in the update sequence. Stored locally as part of state tracking.

### Message Box Independence (Critical)

- **Common message box (`pts`)**: Private chats and basic groups share ONE sequence
- **Channel sequences**: Each channel/supergroup maintains its **own independent `pts`**
- **Secondary sequence (`qts`)**: Secret chats and certain bot events

Your Rust client must maintain:
- One global `pts` (common message box)
- One `pts` per channel (independent)
- One global `seq`
- One global `qts`

grammers handles this internally via the `message_box` module in `grammers-session`.

---

## 2. How Updates Are Delivered

Updates arrive as TL-serialized objects over the persistent connection. The server pushes them proactively — **no polling needed** for channels you're a member of.

### Update Wrapper Types

| Constructor | Description |
|-------------|-------------|
| `updatesTooLong` | Too many pending events — must call `getDifference` |
| `updateShort` | Single low-priority update (no ordering info) |
| `updateShortMessage` | Compact private message format |
| `updateShortChatMessage` | Compact group message format |
| `updates` | Full ordered updates batch (has `seq`) |
| `updatesCombined` | Full ordered updates batch (has `seq_start` and `seq`) |

The short variants "help significantly reduce the transmitted message size for 90% of the updates."

### Processing Order

1. **Phase 1 — Message Box Updates**: Handle all updates with `pts` or `qts`, validating against their respective local states
2. **Phase 2 — Remaining Updates**: Process other updates using `seq`

Clients must **postpone updates received via the socket while filling gaps**.

---

## 3. Channel Message Update Types

### `updateNewChannelMessage`

The primary constructor for new channel/supergroup messages:

```
updateNewChannelMessage#62ba04d9 message:Message pts:int pts_count:int = Update;
```

| Field | Type | Purpose |
|-------|------|---------|
| `message` | `Message` | The new message |
| `pts` | `int` | Channel-specific event count after generation |
| `pts_count` | `int` | Number of events generated |

### `updateNewMessage` vs `updateNewChannelMessage`

| Update | Source | pts sequence |
|--------|--------|-------------|
| `updateNewMessage` | Private chats + basic groups | Shared common pts |
| `updateNewChannelMessage` | Channels + supergroups | Channel's own pts |

In grammers, **both** map to `Update::NewMessage`. You must check `message.peer()` to distinguish.

---

## 4. Gap Recovery

### When Recovery Is Required

- Startup (call `updates.getDifference`)
- Sequence desynchronization (missing updates)
- Server session loss
- Deserialization failures
- Long period without updates (15+ minutes)
- Explicit server request: `updatesTooLong` or `updateChannelTooLong`

### `updates.getDifference`

Recovers gaps in the **common** and **secondary** sequences (`seq` and `qts`).

Response variants:
- `updates.difference`: Full difference, apply and done
- `updates.differenceSlice`: Partial — save intermediate state and repeat
- `updates.differenceTooLong`: Gap too large — reset state

### `updates.getChannelDifference`

Recovers gaps in **channel-specific** sequences.

| Parameter | Details |
|-----------|---------|
| `force` | Skip some updates, reduce server load |
| `channel` | Target `InputChannel` |
| `filter` | `ChannelMessagesFilter` to constrain results |
| `pts` | Current channel `pts` |
| `limit` | Max events — **recommended 10-100** (max 100,000) |

Response variants:
- `channelDifferenceEmpty`: No new updates (has `pts` and optional `timeout`)
- `channelDifference`: Standard response (messages, updates, chats, users)
- `channelDifferenceTooLong`: Gap too large — re-fetch from scratch

When `final` flag is unset, save intermediate `pts` and repeat immediately.

### Error Codes for `getChannelDifference`

| Code | Error | Meaning |
|------|-------|---------|
| 400 | `PERSISTENT_TIMESTAMP_EMPTY/INVALID` | Bad pts value |
| 500 | `PERSISTENT_TIMESTAMP_OUTDATED` | Server replication issue — retry |
| 406 | `CHANNEL_PRIVATE` | Not a member |
| 400 | `CHANNEL_INVALID` | Bad channel reference |

### Timing

- Wait up to **0.5 seconds** before triggering gap recovery (a filling update may arrive)
- **Hard limit**: Poll at most **10 channels simultaneously** via `getChannelDifference`
- For passive listeners: updates are pushed automatically, no polling needed

---

## 5. `UpdatesTooLong` Handling

`updatesTooLong` is a zero-field constructor: *"There are too many events pending to be pushed to the client, so one needs to fetch them manually."*

When received:
1. Call `updates.getDifference` with current local state
2. Process all returned updates
3. If `differenceSlice`, save intermediate state and repeat

`updateChannelTooLong` is the channel-specific equivalent, requiring `getChannelDifference`.

---

## 6. Channels vs Supergroups in the API

Both are represented by the `channel` TL constructor but differ by flags:

| Property | Broadcast Channel | Supergroup |
|----------|------------------|------------|
| `broadcast` flag | `true` | `false` |
| `megagroup` flag | `false` | `true` |
| Members | Unlimited subscribers | Up to 200,000 |
| Who posts | Admins only | All members |
| Own `pts` | Yes | Yes |
| Update type | `updateNewChannelMessage` | `updateNewChannelMessage` |
| grammers `Peer` | `Peer::Channel` | `Peer::Group` |

Additional flags: `gigagroup` (flag 26) for very large supergroups, `forum` (flag 30) for topic-based supergroups.

**For the news alarm use case**: Filter for `Peer::Channel` only (broadcast channels). Skip `Peer::Group` (supergroups/megagroups).

---

## 7. "Restrict Saving Content" (noforwards)

The `noforwards` flag on channels instructs clients to prevent forwarding and saving media.

**Can API clients read the text?** — **Yes.** The message text IS delivered. `noforwards` is a client-side enforcement mechanism. Well-behaved clients prevent forwarding/screenshots, but the `message` field is present in the TL object.

**Caveat**: Using content from `noforwards` channels in ways that bypass protection may violate the API ToS.

---

## 8. Rate Limits

### Passive Update Reception

There are **no documented rate limits** for receiving pushed updates over an open socket. Rate limits apply to **API method calls**, not to pushed updates.

### API Calls

- **General limit**: ~30 requests/second across all operations
- **FLOOD_WAIT**: Error code 420, includes `seconds` to wait
- **Username resolution**: ~200/day
- **`getChannelDifference`**: recommended `limit` of 10-100, max 10 channels polled simultaneously

### For a Channel Listener

A passive listener should encounter **minimal rate limiting** since it mostly receives pushed updates. Rate limits apply to:
- Initial `getDifference` / `getChannelDifference` at startup
- Gap recovery operations
- Any explicit `messages.getHistory` calls

---

## 9. Session Management (Protocol Level)

### Authorization Keys

The **auth_key** is a 2048-bit key created via Diffie-Hellman. It is generated once and almost never changes. Never transmitted over the network.

### Sessions

A **session** is a random 64-bit number. Multiple sessions can share the same auth_key (different app instances). The server may unilaterally forget sessions — clients must handle this gracefully.

### Concurrent Sessions

**No officially documented maximum.** Running one automated session alongside regular Telegram Desktop usage should be fine.

### Session Duration

Configurable: 1 week, 1 month, 3 months, 6 months, or 12 months.

---

## 10. Terms of Service and Automated Reading

### What's Allowed

- Creating Telegram-like messaging applications
- Adding new features or improving existing features
- Personal use of the API for your own account

### What's Prohibited

- Acting "on behalf of the user without the user's knowledge and consent"
- Using data to **train AI or machine learning models**
- Flooding, spamming, faking counters
- Data scraping for creating datasets
- Interfering with "official sponsored messages" when accessing channel content

### Read-Only Risk Assessment

| Activity | Risk | Notes |
|----------|------|-------|
| Passively reading channels you're subscribed to | **Low** | Functionally identical to the official app |
| Running a local LLM on your own messages | **Low** | Personal use, not "training" in the ToS sense |
| Mass scraping/archiving many channels | **High** | Likely considered data scraping |
| Redistributing channel content | **High** | Copyright and ToS violation |

### Key Detail

From the API ID documentation: *"All accounts that log in using unofficial Telegram API clients are automatically put under observation to avoid violations of the Terms of Service."*

### Enforcement

- 10-day remediation window before API access discontinued
- Contact `recover@telegram.org` if wrongfully banned
- Frozen accounts get `FROZEN_METHOD_INVALID` errors on restricted methods

---

## 11. Getting API Credentials (my.telegram.org)

### Known Issues (2025-2026)

- Persistent "ERROR" messages when creating applications
- Each phone number gets **exactly one `api_id`**
- Code sent to Telegram app (not SMS) for portal login
- Workarounds: different browser, VPN, incognito mode, off-peak hours

### Critical Rules

1. `api_hash` is **secret and irrevocable** — Telegram will never let you change it
2. **Never share `api_id`/`api_hash`** — Telegram detects shared credentials
3. Session files bypass credentials — anyone with the `.session` file has full account access
4. Add `*.session` to `.gitignore`
5. Store credentials in environment variables:
   ```bash
   export TELEGRAM_API_ID=12345678
   export TELEGRAM_API_HASH=abcdef0123456789abcdef0123456789
   ```

---

## 12. Startup Sequence for a Rust Client

1. **Open SQLite session** (`SqliteSession::open`)
2. **Create SenderPool** → spawn runner task
3. **Create Client** from handle
4. **Authenticate** if not already authorized (phone + code + optional 2FA)
5. **Iterate all dialogs** (`iter_dialogs()`) — populates peer cache for gap resolution
6. **Start update stream** (`stream_updates()`)
7. **Call `getDifference`** implicitly via `catch_up: true` (or handle manually)
8. **Enter main loop**: process `updates.next()` in a `tokio::select!` with Ctrl+C handling
9. **On shutdown**: call `sync_update_state()`, quit handle, await pool task

### Key Numbers

| Parameter | Value |
|-----------|-------|
| Max API requests/sec | ~30 |
| Max channels to poll simultaneously | 10 |
| Gap recovery wait | 0.5 seconds |
| No-update timeout before recovery | 15 minutes |
| Channel membership limit | 500 (1000 with Premium) |
| Username resolutions/day | ~200 |
| API IDs per phone number | 1 |
| ToS violation remediation window | 10 days |

---

## Sources

- [Telegram API Overview](https://core.telegram.org/api)
- [Working with Updates](https://core.telegram.org/api/updates)
- [Authentication Flow](https://core.telegram.org/api/auth)
- [Obtaining API ID](https://core.telegram.org/api/obtaining_api_id)
- [Channels, Supergroups, Gigagroups](https://core.telegram.org/api/channel)
- [API Terms of Service](https://core.telegram.org/api/terms)
- [Error Handling](https://core.telegram.org/api/errors)
- [MTProto Protocol](https://core.telegram.org/mtproto/description)
- [updateNewChannelMessage](https://core.telegram.org/constructor/updateNewChannelMessage)
- [updates.getChannelDifference](https://core.telegram.org/method/updates.getChannelDifference)
- [Channel Constructor](https://core.telegram.org/constructor/channel)
- [Message Constructor](https://core.telegram.org/constructor/message)
- [Telegram Limits](https://limits.tginfo.me/en)
- [tdlib API ID Ban Issue #534](https://github.com/tdlib/td/issues/534)
- [Telegram API ID Creation Issue #573](https://github.com/tdlib/telegram-bot-api/issues/573)
