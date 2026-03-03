# Rust Crates Reference — Tokenizer, Buffers, Async, Errors

> Pinned versions as of March 2026. Researched from cloned source repos in `/tmp/`.

---

## 1. Tokenizer: `tokenizers` v0.22.2 (HuggingFace)

**Crate**: [`tokenizers`](https://crates.io/crates/tokenizers)
**Repo**: [github.com/huggingface/tokenizers](https://github.com/huggingface/tokenizers)
**Rust source**: `tokenizers/tokenizers/` subdirectory

### Qwen 3.5 27B Tokenizer Facts

| Property | Value |
|----------|-------|
| HuggingFace repo | `Qwen/Qwen3.5-27B` |
| Tokenizer type | **Byte-level BPE** (GPT-2 style, NOT SentencePiece) |
| Tokenizer file | `tokenizer.json` (12.8 MB, self-contained) |
| Vocabulary size | **248,320** tokens |
| Max positions | **262,144** (256K context) |
| Special tokens | 33 (IDs 248044-248076): `<|endoftext|>`, `<|im_start|>`, `<|im_end|>`, `<think>`/`</think>`, etc. |
| Tokenizer class | `Qwen2Tokenizer` |

### Cargo.toml

```toml
# Local file loading only:
tokenizers = "0.22.2"

# With HuggingFace Hub download:
tokenizers = { version = "0.22.2", features = ["http"] }
```

### Features

| Feature | Enables |
|---------|---------|
| `default` | progressbar, onig regex, esaxx_fast |
| `http` | `Tokenizer::from_pretrained()` (downloads from HuggingFace Hub via `hf-hub`) |
| `rustls-tls` | Use rustls instead of native TLS for Hub downloads |

### Loading the Tokenizer

```rust
use tokenizers::Tokenizer;

// Option A: From local file (recommended — bundle tokenizer.json with your app)
let tokenizer = Tokenizer::from_file("tokenizer.json")
    .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {}", e))?;

// Option B: From bytes
let bytes = std::fs::read("tokenizer.json")?;
let tokenizer = Tokenizer::from_bytes(&bytes)
    .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {}", e))?;

// Option C: Download from HuggingFace (requires "http" feature)
let tokenizer = Tokenizer::from_pretrained("Qwen/Qwen3.5-27B", None)
    .map_err(|e| anyhow::anyhow!("failed to download tokenizer: {}", e))?;
```

Source: `tokenizers/src/tokenizer/mod.rs:438-449`

### Encoding API

```rust
// Fast encode (no offset tracking — fastest for token counting)
let encoding = tokenizer.encode_fast("Your text here", false)?;

// Standard encode (with byte-level offsets)
let encoding = tokenizer.encode("Your text here", false)?;

// Batch encode (uses rayon for CPU parallelism)
let encodings = tokenizer.encode_batch(vec!["text1", "text2"], false)?;

// Batch fast encode
let encodings = tokenizer.encode_batch_fast(vec!["text1", "text2"], false)?;
```

Source: `tokenizers/src/tokenizer/mod.rs:786-1324`

Second parameter `add_special_tokens`: whether to add `<|im_start|>` etc. For token counting, use `false`.

### Encoding Struct

```rust
pub struct Encoding {
    ids: Vec<u32>,              // Token IDs
    type_ids: Vec<u32>,
    tokens: Vec<String>,        // Token strings
    words: Vec<Option<u32>>,
    offsets: Vec<Offsets>,       // Byte offsets (empty with encode_fast)
    special_tokens_mask: Vec<u32>,
    attention_mask: Vec<u32>,
    overflowing: Vec<Encoding>,
    sequence_ranges: AHashMap<usize, Range<usize>>,
}

// Key methods:
encoding.len()            // TOKEN COUNT — the number you need
encoding.is_empty()
encoding.get_ids()        // &[u32]
encoding.get_tokens()     // &[String]
```

Source: `tokenizers/src/tokenizer/encoding.rs`

### Token Counting Pattern (For 50k Window)

```rust
fn count_tokens(tokenizer: &Tokenizer, text: &str) -> usize {
    tokenizer.encode_fast(text, false)
        .expect("tokenization should not fail for valid UTF-8")
        .len()
}

fn fits_in_context(tokenizer: &Tokenizer, text: &str, max_tokens: usize) -> bool {
    count_tokens(tokenizer, text) <= max_tokens
}
```

### Performance Notes
- Uses **rayon** for batch parallelism (controlled by `RAYON_RS_NUM_THREADS` env var)
- `encode_fast` avoids offset computation — use when you only need token count
- Compiled with `lto = "fat"` in release profile
- `tokenizer.json` is fully self-contained (model, normalizer, pre-tokenizer, post-processor, decoder)

### Downloading `tokenizer.json`

```bash
# Via hf-hub CLI:
huggingface-cli download Qwen/Qwen3.5-27B tokenizer.json

# Via curl:
curl -L https://huggingface.co/Qwen/Qwen3.5-27B/resolve/main/tokenizer.json -o tokenizer.json

# Or use hf-hub crate in Rust:
use hf_hub::api::sync::Api;
let api = Api::new()?;
let path = api.model("Qwen/Qwen3.5-27B".into()).get("tokenizer.json")?;
```

The `hf-hub` crate (v0.5.0) is bundled inside `tokenizers` when the `http` feature is enabled.

### Alternatives Investigated (Not Recommended)

| Crate | Version | Why Not |
|-------|---------|---------|
| `tiktoken-rs` | 0.9.1 | **OpenAI only** — no Qwen support. Only has O200k, Cl100k, R50k, P50k encodings. |
| `bpe-qwen` | 0.1.5 | **Python package only** (PyO3 cdylib). Not published on crates.io. |
| `sentencepiece` | 0.13.1 | Qwen uses BPE, not SentencePiece. Wrong tokenizer type. |

---

## 2. Circular Buffer: `circular-buffer` v1.2.0

**Crate**: [`circular-buffer`](https://crates.io/crates/circular-buffer)

### Why This Over Alternatives

| Crate | Verdict | Why |
|-------|---------|-----|
| **`circular-buffer` v1.2.0** | **CHOSEN** | Const-generic capacity, auto-eviction, `boxed()` for heap, rich iteration |
| `VecDeque` (std) | Fallback | No auto-eviction (manual 2-line check), capacity not enforced |
| `ringbuf` v0.4.9 | Not suitable | SPSC-oriented, `push_overwrite()` needs `&mut self`, can't iterate stored items through split |
| `heapless` v0.9.2 | Overkill | Aimed at embedded/no-std, less ergonomic |
| `bounded-vec-deque` v0.1.1 | Too young | v0.1.1, minimal maturity |

### API

```rust
use circular_buffer::CircularBuffer;

// Heap-allocated 512-element buffer (avoids stack overflow)
let mut buf: Box<CircularBuffer<512, MyMessage>> = CircularBuffer::boxed();

// Push (auto-evicts oldest when full)
buf.push_back(message);                // Returns Option<T> if evicted
let evicted = buf.push_back(message);  // Some(old_msg) if buffer was full

// Access
buf.front()          // -> Option<&T>  (oldest)
buf.back()           // -> Option<&T>  (newest)
buf.get(index)       // -> Option<&T>
buf.len()            // -> usize
buf.capacity()       // -> usize  (always 512)
buf.is_empty()       // -> bool
buf.is_full()        // -> bool

// Iteration (in order: oldest → newest)
for msg in buf.iter() { ... }
buf.iter().rev()     // newest → oldest

// Slicing
let (a, b) = buf.as_slices();  // Two contiguous slices (ring buffer wraps)
buf.make_contiguous();          // Rearrange to single contiguous slice

// Bulk
buf.clear();
buf.drain(..);       // Remove and yield all elements
buf.to_vec();        // Clone to Vec<T>
buf.extend_from_slice(&items);
```

### Capacity Is Compile-Time Const

```rust
CircularBuffer::<512, T>::boxed()  // Capacity is 512, enforced at compile time
```

No runtime check needed — you literally cannot exceed the capacity. Pushing to a full buffer silently evicts the oldest element.

### Stack vs Heap

- `CircularBuffer::new()` — stack allocated (danger: 512 × sizeof(T) bytes on stack)
- `CircularBuffer::boxed()` — **heap allocated via Box** (use this for large buffers)

---

## 3. Async Runtime: `tokio` v1

**Crate**: [`tokio`](https://crates.io/crates/tokio) v1.49.0 (latest stable)
**LTS**: v1.43.x (until March 2026), v1.47.x (until September 2026)

### Feature Flags

grammers already pulls in tokio with `["rt", "macros", "io-util", "sync", "time", "net"]`. For your application, use:

```toml
tokio = { version = "1", features = ["rt-multi-thread", "macros", "time", "sync", "signal"] }
```

Or simply `features = ["full"]` to get everything.

### Spawning the LLM Processing Task

```rust
let buffers = Arc::clone(&shared_buffers);
let llm_task = tokio::spawn(async move {
    loop {
        // 1. Snapshot buffers under read lock
        let snapshot = {
            let guard = buffers.read().unwrap();
            guard.clone() // or selective cloning
        };

        // 2. Build prompt, tokenize, call LLM
        // ...

        // 3. No sleep — iterate as fast as LLM responds
    }
});
```

### Signal Handling

```rust
tokio::select! {
    _ = tokio::signal::ctrl_c() => {
        println!("Shutting down...");
    }
    // ... other branches
}
```

---

## 4. Thread-Safe Sharing

### Recommended Pattern: `Arc<std::sync::RwLock<HashMap<i64, Buffer>>>`

For a Telegram news system with moderate message rates (messages/second, not millions), a single `RwLock` wrapping the entire map is sufficient:

```rust
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use circular_buffer::CircularBuffer;

struct ChannelMessage {
    id: i32,
    text: String,
    date: chrono::DateTime<chrono::Utc>,
    channel_title: String,
    channel_id: i64,
}

type ChannelBuffer = Box<CircularBuffer<512, ChannelMessage>>;
type SharedBuffers = Arc<RwLock<HashMap<i64, ChannelBuffer>>>;
```

**Why `std::sync::RwLock` over `tokio::sync::RwLock`?**
Buffer operations (push a message, iterate to format) are fast and synchronous — no `.await` inside the critical section. `std::sync::RwLock` is faster for short critical sections.

**Why `RwLock` over `Mutex`?**
The reader (LLM formatting) can hold a read lock without blocking incoming writes from the Telegram update task (assuming they target different channels — but actually with `RwLock<HashMap>` the whole map is locked, so `Mutex` would work equally well). With this workload, both are fine.

### Alternative: `DashMap` (If Contention Becomes an Issue)

```toml
dashmap = "6.1"
```

```rust
use dashmap::DashMap;

type SharedBuffers = Arc<DashMap<i64, ChannelBuffer>>;

// Writer:
buffers.entry(channel_id).or_insert_with(CircularBuffer::boxed).push_back(msg);

// Reader:
for entry in buffers.iter() {
    let channel_id = *entry.key();
    let buffer = entry.value();
    // format buffer contents...
}
```

`DashMap` uses internal sharding with `parking_lot` locks, reducing contention when accessing different keys. But for Telegram message rates, this optimization is unnecessary.

### `parking_lot` v0.12.5

Faster `Mutex`/`RwLock` than std (1.5x–5x under contention, though std has narrowed the gap with futex on Linux). Only needed if profiling shows lock contention.

---

## 5. Error Handling: `anyhow` v1.0.102

**Crate**: [`anyhow`](https://crates.io/crates/anyhow) (by David Tolnay, same author as serde)

### Why `anyhow` (For "Errors Contain Full Context, No Silent Fallbacks")

| Requirement | How anyhow Satisfies It |
|-------------|------------------------|
| Dynamic string context | `.with_context(\|\| format!("failed for channel {}", id))` |
| Full error chain | Every `.context()` wraps the previous error |
| No silent fallbacks | `bail!("unexpected: {}", detail)` returns immediately |
| Logic assertions | `ensure!(condition, "invariant violated: {}", desc)` |
| Backtraces | Automatic on Rust ≥ 1.65 (set `RUST_BACKTRACE=1`) |

### Key API

```rust
use anyhow::{anyhow, bail, ensure, Context, Result};

// Type alias
fn do_thing() -> Result<()> { ... }  // Result<T, anyhow::Error>

// Create errors
anyhow!("something went wrong: {}", detail)
bail!("fatal: channel {} not found", channel_id)  // returns Err immediately
ensure!(buffer.len() <= 512, "buffer overflow: len={}", buffer.len())

// Chain context
let data = std::fs::read("file.txt")
    .with_context(|| format!("failed to read config at {}", path))?;

// Downcast
if let Some(io_err) = err.downcast_ref::<std::io::Error>() { ... }

// Full chain iteration
for cause in err.chain() {
    eprintln!("  caused by: {}", cause);
}
```

### When to Add `thiserror` v2.0.18

If you need callers to match on specific error variants (e.g., "is this a network error or a config error?"), define a `thiserror` enum. It integrates seamlessly with `anyhow` via `#[from]`:

```rust
#[derive(thiserror::Error, Debug)]
enum TelegramError {
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("connection lost")]
    Disconnected,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
```

---

## 6. Timestamps: `chrono` v0.4

**Crate**: [`chrono`](https://crates.io/crates/chrono) v0.4.44

Already a transitive dependency of `grammers-client` — adds zero compilation overhead.

### Key Usage

```rust
use chrono::{DateTime, Utc};

// grammers messages already provide:
let date: DateTime<Utc> = message.date();

// Current time:
let now = Utc::now();

// Duration calculations:
let age = now - date;
if age.num_minutes() <= 15 {
    // Within the 15-minute window
}

// Formatting:
date.format("%Y-%m-%d %H:%M:%S UTC").to_string()
date.to_rfc3339()

// From Unix timestamp:
DateTime::from_timestamp(secs, 0)
```

---

## 7. JSON: `serde` v1 + `serde_json` v1

```toml
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

### For LLM JSON Output Parsing

```rust
use serde::Deserialize;

#[derive(Deserialize, Debug)]
struct NewsUpdate {
    updates: Vec<NewsItem>,
}

#[derive(Deserialize, Debug)]
struct NewsItem {
    channel: String,
    headline: String,
    importance: String, // "high", "medium", "low"
    summary: String,
}

// Parse LLM output:
let response: NewsUpdate = serde_json::from_str(&llm_output)
    .with_context(|| format!("failed to parse LLM JSON output: {}", &llm_output[..200.min(llm_output.len())]))?;
```

### For Ollama Request/Response

```rust
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct OllamaRequest {
    model: String,
    messages: Vec<OllamaMessage>,
    stream: bool,
    options: OllamaOptions,
}

#[derive(Serialize)]
struct OllamaOptions {
    num_ctx: u64,
    num_predict: u64,
    temperature: f32,
    top_k: u32,
    top_p: f32,
    presence_penalty: f32,
    repeat_penalty: f32,
}

#[derive(Deserialize)]
struct OllamaResponse {
    model: String,
    message: OllamaMessage,
    done: bool,
    #[serde(default)]
    total_duration: Option<u64>,  // nanoseconds
    #[serde(default)]
    eval_count: Option<u64>,      // tokens generated
}
```

---

## 8. HTTP Client: `reqwest` v0.12

```toml
reqwest = { version = "0.12", features = ["json", "stream"] }
futures-util = "0.3"   # for StreamExt
```

### Streaming Ollama Response

```rust
use futures_util::StreamExt;

let response = reqwest::Client::new()
    .post("http://localhost:11434/api/chat")
    .json(&request)
    .send()
    .await
    .context("failed to send request to Ollama")?;

ensure!(response.status().is_success(),
    "Ollama returned HTTP {}", response.status());

let mut stream = response.bytes_stream();
let mut full_content = String::new();
let mut buffer = String::new();

while let Some(chunk) = stream.next().await {
    let chunk = chunk.context("stream read error")?;
    buffer.push_str(&String::from_utf8_lossy(&chunk));

    while let Some(newline_pos) = buffer.find('\n') {
        let line = buffer[..newline_pos].trim().to_string();
        buffer = buffer[newline_pos + 1..].to_string();

        if !line.is_empty() {
            let resp: OllamaResponse = serde_json::from_str(&line)
                .with_context(|| format!("bad Ollama JSON: {}", &line[..100.min(line.len())]))?;
            full_content.push_str(&resp.message.content);
            if resp.done {
                break;
            }
        }
    }
}
```

---

## 9. String Formatting Efficiency

### Pattern: `std::fmt::Write` + `String::with_capacity`

```rust
use std::fmt::Write as _;

// Pre-allocate based on expected output size
let estimated_capacity = buffer.len() * 256; // ~256 bytes per message
let mut output = String::with_capacity(estimated_capacity);

for msg in buffer.iter() {
    // write! appends directly — no intermediate String allocation
    writeln!(&mut output, "[{}] {}: {}",
        msg.date.format("%H:%M:%S"),
        msg.channel_title,
        msg.text
    ).unwrap(); // write! to String never fails
}
```

**Why not `format!`?**
`format!()` allocates a new `String` per call. `write!()` appends to an existing `String`, avoiding N allocations for N messages.

**Why `.unwrap()` is safe here:**
Writing to a `String` via `fmt::Write` returns `fmt::Result` which is always `Ok(())` — it cannot fail.

### Capacity Estimation

For 512 messages with timestamps, sender names, and text:
```rust
// "[HH:MM:SS] ChannelName: message text\n" ≈ 80-300 bytes
let estimated = buffer.len() * 256;
let mut output = String::with_capacity(estimated);
```

---

## 10. Logging: `log` v0.4 + `simple_logger` v5.0

```toml
log = "0.4"
simple_logger = "5.0"
```

```rust
simple_logger::init_with_level(log::Level::Warn)?;

// In code:
log::info!("Connected to Telegram, monitoring {} channels", count);
log::warn!("No updates for 5 minutes — possible hang");
log::error!("Update stream error: {:#}", err);
log::debug!("Received message {} from channel {}", msg.id(), ch.title());
```

---

## 11. Full Dependency Summary

```toml
[dependencies]
# Telegram (grammers ecosystem)
grammers-client = "0.8.1"
grammers-session = "0.8.0"
grammers-mtsender = "0.8.1"
grammers-tl-types = "0.8.0"

# Async runtime
tokio = { version = "1", features = ["rt-multi-thread", "macros", "time", "sync", "signal"] }

# HTTP + streaming
reqwest = { version = "0.12", features = ["json", "stream"] }
futures-util = "0.3"

# Tokenizer
tokenizers = "0.22.2"

# Data structures
circular-buffer = "1.2"

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Timestamps (transitive dep of grammers)
chrono = "0.4"

# Errors
anyhow = "1"

# Logging
log = "0.4"
simple_logger = "5.0"
```
