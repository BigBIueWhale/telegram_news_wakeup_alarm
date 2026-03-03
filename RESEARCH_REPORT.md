# Telegram News Wakeup Alarm — Rust Research Report

> **Date**: March 3, 2026
> **System**: Ubuntu with Snap-installed Telegram Desktop 6.6.1 (revision 6906)
> **LLM Backend**: Qwen 3.5 27B (Q4_K_M) via Ollama on RTX 5090 — `http://localhost:11434`
> **Modelfile**: `qwen3.5-custom` — `num_ctx: 131072`, `num_predict: 81920`, `temperature: 1.0`, `top_k: 20`, `top_p: 0.95`, `presence_penalty: 1.5`, `repeat_penalty: 1.0` — template `{{ .Prompt }}` (no system prompt)
> **Language**: Rust
> **Project Directory**: `/home/user/Desktop/telegram_news_wakeup_alarm`

---

## User Prompts (Verbatim)

**Prompt 1:**
> I have Telegram Desktop installed on this computer. Is it possible for you to subscribe to all received messages live on all channels programmatically in order to send the text to an LLM? See '/home/user/shared_vm/qwen3_5_27b_research/solution.md' that's **also** already set up on this PC. Research extensively online and create an extremely thorough report. During your research download any repos or sources you need to view / investigate / search into /tmp for more efficient lookup

**Prompt 2:**
> Our workspace folder is '/home/user/Desktop/telegram_news_wakeup_alarm', just to be clear. And the research must be incredibly information-generous and well researched. Research **way more** for follow up questions and gotchas and everything else for my usecase- most up to dare telegram version installed with snap. Use subagents for the research. In the final report- do include my prompts dueing this conversation- verbatim (obviously not inxluding this specific sentence)

**Prompt 3:**
> Create an extremely robust (errors contain full context- dynamic string based, no silent fallbacks for logic assertions) Rust utility that just sort of runs and subscribes to all messages on all Telegram channels that I'm subscribed to. It should collect these messages by timestamp UTC time convention in an in-memory structure, sliding window that contains up to 512 messages **from each channel**. And when I say channel I mean channel, not personal conversation or group chat. So really it's a sliding window buffer (or max size circulae buffer) for each channel! Obviously as new messages come, the old ones get flushed out. Separately another thread should also have access to these memory buffers and should be there in a loop reading the contents of all the buffers and converting into friendly string, asking an LLM what's the importanr news updates that occurred over the last 15 minutes. To stay within context, use qwen3.5 tokenizer (preferably accurately and high performance within Rust!) then increase the nunber of minutes you take back until either you're feeding all buffers to the LLM or until the context is **less** than 50k tokens. All this context is only to give the LLM an idea about how things look like. In the **prompt** (there is no system prompt for this model) we ask the model to only focus on the last 15 minutes (going back from the newest message's date within the current buffers). This is sort of like binary search- increasing the time we go back, constructing the prompt each time usng efficient Rust code, and tokenizing to maximize the fit efficiently and greedily, and uniformly across channels (same amount of history for each channel). At every iteration of the loop (which happens as fast as the LLM is at ingesting + thinking + responding) we ask the LLM to output text in a parseable format (json for example) that includes information-dense, information-generous, incredibly brief updates of what's happening right now! Essentially the output of each iteration of this loop is a string. This string should be printed to stdout. First- read '/home/user/Desktop/telegram_news_wakeup_alarm/RESEARCH_REPORT.md' and '/home/user/Desktop/telegram_news_wakeup_alarm/TELETHON_EDGE_CASES_RESEARCH.md'. The very first step before all this- take my instructions verbatim and write to a file within this project's folder. Go!

**Prompt 4:**
> Update the research reports to be **entirely** Rust-focused and incredibly up to date (and pinned to versions). As part of your research, feel free to download sources (clone) into /tmp for more thorough investigation of specific up to date pinned versions of software libraries. Please update'/home/user/Desktop/telegram_news_wakeup_alarm/RESEARCH_REPORT.md' and '/home/user/Desktop/telegram_news_wakeup_alarm/TELETHON_EDGE_CASES_RESEARCH.md' based on your detailed research accordingly. And actually refactor '/home/user/Desktop/telegram_news_wakeup_alarm/RESEARCH_REPORT.md' into smaller files. All the while, during your research focus on what I'll actually care about for my usecase-'/home/user/Desktop/telegram_news_wakeup_alarm/INSTRUCTIONS_VERBATIM.md' and the information I'll need to later tackle that problem myself.

---

## TL;DR — The Answer

**Yes.** Use the **grammers** Rust library (by Lonami, same author as Python's Telethon) to connect to Telegram's MTProto API as a user account, subscribe to real-time channel message updates, buffer them per-channel in `circular-buffer` ring buffers, tokenize with HuggingFace's `tokenizers` crate (loads Qwen 3.5's `tokenizer.json` natively), and send prompts to Ollama's `/api/chat` endpoint via raw `reqwest`.

### Core Stack (Pinned Versions)

| Component | Crate | Version | Purpose |
|-----------|-------|---------|---------|
| Telegram MTProto | `grammers-client` | **0.8.1** | User-account message subscription |
| Telegram session | `grammers-session` | **0.8.0** | SQLite-backed session persistence |
| Telegram sender | `grammers-mtsender` | **0.8.1** | Network transport + sender pool |
| Telegram types | `grammers-tl-types` | **0.8.0** | Generated TL type definitions |
| Async runtime | `tokio` | **1** (features: full) | Multi-threaded async executor |
| HTTP client | `reqwest` | **0.12** (features: json, stream) | Ollama API calls |
| Tokenizer | `tokenizers` | **0.22.2** | Qwen 3.5 BPE token counting |
| Ring buffer | `circular-buffer` | **1.2** | Per-channel 512-message sliding window |
| JSON | `serde` + `serde_json` | **1** / **1** | Request/response serialization |
| Timestamps | `chrono` | **0.4** | UTC message timestamps (transitive dep) |
| Error handling | `anyhow` | **1** | Dynamic context-rich error chains |
| Logging | `log` + `simple_logger` | **0.4** / **5.0** | Structured logging |

### Architecture Overview

```
┌──────────────────────────────────────────────────────────┐
│  Telegram MTProto (grammers-client)                      │
│  - Authenticates as user account                         │
│  - Receives UpdateNewChannelMessage in real-time         │
│  - Filters: only Peer::Channel (broadcast), skip groups  │
└────────────────────────┬─────────────────────────────────┘
                         │ Update::NewMessage
                         ▼
┌──────────────────────────────────────────────────────────┐
│  Per-Channel Circular Buffers                            │
│  HashMap<i64, CircularBuffer<512, ChannelMessage>>       │
│  Protected by Arc<std::sync::RwLock<...>>                │
│  - Writer: Telegram update task pushes messages          │
│  - Reader: LLM processing task reads snapshots           │
└────────────────────────┬─────────────────────────────────┘
                         │ Snapshot (clone under read lock)
                         ▼
┌──────────────────────────────────────────────────────────┐
│  LLM Processing Loop (separate tokio task)               │
│  1. Snapshot all buffers                                 │
│  2. Binary search: expand time window from 15min         │
│  3. Uniform per-channel history selection                │
│  4. Tokenize with Qwen 3.5 tokenizer (< 50k tokens)     │
│  5. Build prompt (no system prompt — raw user message)   │
│  6. POST to Ollama /api/chat (stream response)           │
│  7. Parse JSON output, print to stdout                   │
│  8. Repeat immediately                                   │
└────────────────────────┬─────────────────────────────────┘
                         │ JSON news updates
                         ▼
                      stdout
```

---

## Report Files

This report is split into focused reference documents:

| File | Contents |
|------|----------|
| **[GRAMMERS_REFERENCE.md](./GRAMMERS_REFERENCE.md)** | grammers library API, types, authentication, update handling, session management, version pinning |
| **[GRAMMERS_EDGE_CASES.md](./GRAMMERS_EDGE_CASES.md)** | Known bugs, gotchas, edge cases for long-running listeners, risk assessment, workarounds |
| **[RUST_CRATES_REFERENCE.md](./RUST_CRATES_REFERENCE.md)** | Tokenizer, circular buffer, async runtime, error handling, string formatting crates |
| **[OLLAMA_API_REFERENCE.md](./OLLAMA_API_REFERENCE.md)** | Ollama `/api/chat` format, streaming, Rust integration, `ollama-rs` vs raw `reqwest` |
| **[TELEGRAM_PROTOCOL_REFERENCE.md](./TELEGRAM_PROTOCOL_REFERENCE.md)** | MTProto update system (pts/seq/qts), gap recovery, channels vs supergroups, ToS, rate limits |
| **[INSTRUCTIONS_VERBATIM.md](./INSTRUCTIONS_VERBATIM.md)** | Original user requirements verbatim |

---

## Recommended `Cargo.toml`

```toml
[package]
name = "telegram-news-wakeup-alarm"
version = "0.1.0"
edition = "2024"

[dependencies]
# Telegram MTProto client (by Lonami, same author as Telethon)
grammers-client = "0.8.1"
grammers-session = "0.8.0"
grammers-mtsender = "0.8.1"
grammers-tl-types = "0.8.0"

# Async runtime (grammers requires tokio)
tokio = { version = "1", features = ["rt-multi-thread", "macros", "time", "sync", "signal"] }

# HTTP client for Ollama API (raw reqwest — ollama-rs lacks presence_penalty)
reqwest = { version = "0.12", features = ["json", "stream"] }
futures-util = "0.3"

# Qwen 3.5 tokenizer (byte-level BPE, loads tokenizer.json)
tokenizers = "0.22.2"

# Per-channel 512-message circular buffer
circular-buffer = "1.2"

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Timestamps (already a transitive dep of grammers-client)
chrono = "0.4"

# Error handling — dynamic context-rich errors, no silent fallbacks
anyhow = "1"

# Logging
log = "0.4"
simple_logger = "5.0"
```

### Why raw `reqwest` over `ollama-rs`?

`ollama-rs` v0.3.4 does **not** support `presence_penalty` in its `ModelOptions` struct (all fields are `pub(super)`). Since the Modelfile requires `presence_penalty: 1.5`, we must use raw `reqwest` to construct the `/api/chat` JSON body with full control. See [OLLAMA_API_REFERENCE.md](./OLLAMA_API_REFERENCE.md) for complete request/response format.

### Why `tokenizers` over alternatives?

- Qwen 3.5 uses **byte-level BPE** (GPT-2 style), not SentencePiece
- `tokenizers` v0.22.2 loads `tokenizer.json` natively — the exact file Qwen publishes on HuggingFace
- `tiktoken-rs` only supports OpenAI tokenizers (no Qwen support)
- `encode_fast()` skips offset computation for maximum token-counting speed
- Vocab size: **248,320 tokens**, max context: **262,144 positions**
- See [RUST_CRATES_REFERENCE.md](./RUST_CRATES_REFERENCE.md) for full API

### Why `circular-buffer` over `VecDeque`?

- Const-generic capacity `CircularBuffer<512, T>` — capacity enforced at compile time
- Auto-eviction: `push_back()` on a full buffer silently drops the oldest element
- `boxed()` method for heap allocation (avoids stack overflow with 512 entries)
- `VecDeque` requires manual `if len > 512 { pop_front() }` — easy to forget
- See [RUST_CRATES_REFERENCE.md](./RUST_CRATES_REFERENCE.md) for comparison

---

## Key Decisions and Gotchas Summary

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Library | grammers (not tdlib-rs) | Pure Rust, same author as Telethon, no C++ build dependency |
| Session storage | SqliteSession | Persists auth key, peer cache, update state across restarts |
| Buffer sharing | `Arc<std::sync::RwLock<HashMap<i64, Buffer>>>` | Simple, sufficient for Telegram message rates |
| Ollama endpoint | `/api/chat` (native) | Must set `num_ctx`, `top_k`, `presence_penalty` — impossible via `/v1/chat/completions` |
| Prompt format | User role only (no system) | Model template is `{{ .Prompt }}` — no system prompt support |
| Streaming | Yes (newline-delimited JSON) | Lower latency, can print partial results |
| Token counting | `tokenizer.encode_fast()` | Skips offset tracking, fastest path to `.len()` |

### Critical Gotchas (see [GRAMMERS_EDGE_CASES.md](./GRAMMERS_EDGE_CASES.md) for full details)

| # | Gotcha | Severity | Mitigation |
|---|--------|----------|------------|
| 1 | **Gap-on-update trickle bug** — updates queue indefinitely if trickle pattern prevents 15min timeout | HIGH | Wrap `next()` in `tokio::time::timeout()`, rebuild client on timeout |
| 2 | **UpdateStream permanent hang** — fatal I/O error causes `Poll::Pending` forever | HIGH | Always use `tokio::time::timeout()` around `updates.next()` |
| 3 | **Must call `iter_dialogs()` at startup** — without it, channel hashes are missing and gap recovery fails silently | HIGH | Call once after authentication, before entering update loop |
| 4 | **`sync_update_state()` not automatic** — update state lost on crash if not called | MEDIUM | Call periodically + on graceful shutdown (Ctrl+C handler) |
| 5 | **No built-in reconnection** — connection drop requires manual client rebuild | MEDIUM | Outer reconnection loop with exponential backoff |
| 6 | **`ollama-rs` missing `presence_penalty`** — can't use the crate for this Modelfile | MEDIUM | Use raw `reqwest` instead |
| 7 | **Default update queue limit is 100** — slow handler drops updates | LOW | Increase `update_queue_limit` or spawn handler tasks |
| 8 | **Telegram may force-logout all sessions** on detecting MTProto library | LOW | Change cloud password after first login; avoid spammy patterns |

---

## Sources & Repos Investigated

Repos cloned to `/tmp/` during research:
- `/tmp/grammers/` — grammers source (Lonami/grammers)
- `/tmp/tokenizers/` — HuggingFace tokenizers Rust core
- `/tmp/ollama-rs/` — ollama-rs client library
- `/tmp/tiktoken-rs/` — tiktoken-rs (ruled out — OpenAI only)
- `/tmp/ringbuf/` — ringbuf crate (ruled out — SPSC oriented)

Key references:
- [grammers on Codeberg](https://codeberg.org/Lonami/grammers) (active repo)
- [grammers on GitHub](https://github.com/Lonami/grammers) (archived Feb 2026)
- [grammers crates.io](https://crates.io/crates/grammers-client)
- [Telegram MTProto API](https://core.telegram.org/api)
- [Telegram Update System](https://core.telegram.org/api/updates)
- [HuggingFace tokenizers](https://github.com/huggingface/tokenizers)
- [Qwen/Qwen3.5-27B on HuggingFace](https://huggingface.co/Qwen/Qwen3.5-27B)
- [Ollama API docs](https://github.com/ollama/ollama/blob/main/docs/api.md)
- [circular-buffer crate](https://crates.io/crates/circular-buffer)
