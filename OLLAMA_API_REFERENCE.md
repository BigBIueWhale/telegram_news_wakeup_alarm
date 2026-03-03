# Ollama API Reference — Rust Integration

> **Ollama endpoint**: `http://localhost:11434`
> **Model**: `qwen3.5-custom` (Qwen 3.5 27B Q4_K_M)
> **Modelfile**: `FROM qwen3.5:27b-q4_K_M` with custom parameters
> **Template**: `{{ .Prompt }}` (raw passthrough — no system prompt)

---

## 1. Why `/api/chat` (Native) Over `/v1/chat/completions` (OpenAI-Compatible)

The OpenAI-compatible endpoint (`/v1/chat/completions`) **cannot** set these critical parameters:
- `num_ctx` (context window size)
- `top_k`
- `min_p`
- `repeat_penalty`

Since the Modelfile requires `num_ctx: 131072`, `top_k: 20`, and `repeat_penalty: 1.0`, the native `/api/chat` endpoint is **mandatory**.

---

## 2. Why Raw `reqwest` Over `ollama-rs`

`ollama-rs` v0.3.4 (latest, Feb 2026) does **not** support `presence_penalty` in its `ModelOptions` struct. The fields are:

```rust
// From ollama-rs/src/models.rs:62-96
// Fields present: mirostat, mirostat_eta, mirostat_tau, num_ctx, num_gqa, num_gpu,
//                 num_thread, repeat_last_n, repeat_penalty, temperature, seed,
//                 stop, tfs_z, num_predict, top_k, top_p
// MISSING: presence_penalty, frequency_penalty
```

All fields are `pub(super)` — you cannot extend the struct without forking. Since `presence_penalty: 1.5` is a required parameter in our Modelfile, we must use raw `reqwest`.

### `ollama-rs` Quick Reference (If It Gets Updated)

If a future `ollama-rs` version adds `presence_penalty`:
```toml
ollama-rs = { version = "0.3.4", features = ["stream"] }
```
```rust
use ollama_rs::Ollama;
use ollama_rs::generation::chat::{ChatMessage, ChatMessageRequest};
use ollama_rs::models::ModelOptions;

let ollama = Ollama::default(); // http://127.0.0.1:11434
let opts = ModelOptions::default()
    .num_ctx(131072)
    .num_predict(81920)
    .temperature(1.0)
    .top_k(20)
    .top_p(0.95);
    // .presence_penalty(1.5) — NOT AVAILABLE

let req = ChatMessageRequest::new("qwen3.5-custom".into(), messages).options(opts);
let stream = ollama.send_chat_messages_stream(req).await?;
```

---

## 3. `/api/chat` Request Format

### Full Request JSON

```json
{
    "model": "qwen3.5-custom",
    "messages": [
        {
            "role": "user",
            "content": "Your prompt text here"
        }
    ],
    "stream": true,
    "options": {
        "num_ctx": 131072,
        "num_predict": 81920,
        "temperature": 1.0,
        "top_k": 20,
        "top_p": 0.95,
        "presence_penalty": 1.5,
        "repeat_penalty": 1.0
    }
}
```

**Important**: Since the model template is `{{ .Prompt }}` (no system prompt), everything goes in a single `"user"` role message. There is no `"system"` role for this model.

### All Valid `options` Parameters

| Parameter | Type | Default | Our Value | Description |
|-----------|------|---------|-----------|-------------|
| `num_ctx` | int | 2048 | **131072** | Context window size in tokens |
| `num_predict` | int | -1 | **81920** | Max tokens to generate (-1 = infinite) |
| `temperature` | float | 0.8 | **1.0** | Creativity/randomness |
| `top_k` | int | 40 | **20** | Limit token selection pool |
| `top_p` | float | 0.9 | **0.95** | Nucleus sampling threshold |
| `presence_penalty` | float | 0.0 | **1.5** | Fixed penalty for tokens that appeared |
| `repeat_penalty` | float | 1.0 | **1.0** | Repetition penalty |
| `min_p` | float | 0.0 | — | Minimum probability threshold |
| `frequency_penalty` | float | 0.0 | — | Proportional frequency-based penalty |
| `repeat_last_n` | int | 64 | — | How far back to look for repetition |
| `seed` | int | 0 | — | Random seed (0 = random) |
| `stop` | string[] | — | — | Stop sequences |
| `num_keep` | int | — | — | Tokens to retain in context |
| `mirostat` | int | 0 | — | Mirostat sampling (0=off) |
| `num_gpu` | int | — | — | GPU layer count |
| `num_thread` | int | — | — | CPU thread count |
| `num_batch` | int | 2 | — | Batch size |

---

## 4. Streaming Response Format

Ollama streams **newline-delimited JSON**. Each line is a complete JSON object.

### Intermediate Chunks (content tokens)

```json
{"model":"qwen3.5-custom","created_at":"2026-03-03T12:00:00.123Z","message":{"role":"assistant","content":"The"},"done":false}
```

### Final Chunk (`done: true`)

```json
{
    "model": "qwen3.5-custom",
    "created_at": "2026-03-03T12:00:05.456Z",
    "message": {"role": "assistant", "content": ""},
    "done": true,
    "total_duration": 4883583458,
    "load_duration": 1334875,
    "prompt_eval_count": 26,
    "prompt_eval_duration": 342546000,
    "eval_count": 282,
    "eval_duration": 4535599000
}
```

Duration values are in **nanoseconds**.

- `total_duration`: Wall-clock time for the entire request
- `load_duration`: Time to load the model into memory (0 if already loaded)
- `prompt_eval_count`: Number of tokens in the prompt
- `prompt_eval_duration`: Time to process the prompt
- `eval_count`: Number of tokens generated
- `eval_duration`: Time to generate the response

### Non-Streaming Response (`"stream": false`)

Single JSON object with the complete response and all timing data.

---

## 5. Rust Types for Raw `reqwest` Integration

```rust
use serde::{Deserialize, Serialize};

// ── Request Types ──

#[derive(Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaMessage>,
    stream: bool,
    options: OllamaOptions,
}

#[derive(Serialize, Deserialize, Clone)]
struct OllamaMessage {
    role: String,     // "user", "assistant", "system", "tool"
    content: String,
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

// ── Response Types ──

#[derive(Deserialize)]
struct OllamaChatResponse {
    model: String,
    created_at: String,
    message: OllamaMessage,
    done: bool,
    #[serde(default)]
    total_duration: Option<u64>,      // nanoseconds
    #[serde(default)]
    load_duration: Option<u64>,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    prompt_eval_duration: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
    #[serde(default)]
    eval_duration: Option<u64>,
}
```

---

## 6. Complete Streaming Call Pattern

```rust
use anyhow::{ensure, Context, Result};
use futures_util::StreamExt;
use reqwest::Client;

const OLLAMA_URL: &str = "http://localhost:11434/api/chat";

async fn query_ollama(prompt: &str) -> Result<String> {
    let request = OllamaChatRequest {
        model: "qwen3.5-custom".to_string(),
        messages: vec![OllamaMessage {
            role: "user".to_string(),
            content: prompt.to_string(),
        }],
        stream: true,
        options: OllamaOptions {
            num_ctx: 131072,
            num_predict: 81920,
            temperature: 1.0,
            top_k: 20,
            top_p: 0.95,
            presence_penalty: 1.5,
            repeat_penalty: 1.0,
        },
    };

    let response = Client::new()
        .post(OLLAMA_URL)
        .json(&request)
        .send()
        .await
        .context("failed to connect to Ollama at localhost:11434")?;

    ensure!(
        response.status().is_success(),
        "Ollama returned HTTP {}: {}",
        response.status(),
        response.text().await.unwrap_or_default()
    );

    let mut stream = response.bytes_stream();
    let mut full_content = String::new();
    let mut line_buffer = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Ollama stream read error")?;
        line_buffer.push_str(&String::from_utf8_lossy(&chunk));

        // Process complete lines (newline-delimited JSON)
        while let Some(newline_pos) = line_buffer.find('\n') {
            let line = line_buffer[..newline_pos].trim().to_string();
            line_buffer = line_buffer[newline_pos + 1..].to_string();

            if line.is_empty() {
                continue;
            }

            let resp: OllamaChatResponse = serde_json::from_str(&line)
                .with_context(|| {
                    format!(
                        "failed to parse Ollama response JSON: {}",
                        &line[..200.min(line.len())]
                    )
                })?;

            full_content.push_str(&resp.message.content);

            if resp.done {
                if let Some(eval_count) = resp.eval_count {
                    if let Some(eval_duration) = resp.eval_duration {
                        let tokens_per_sec =
                            eval_count as f64 / (eval_duration as f64 / 1_000_000_000.0);
                        log::info!(
                            "Generated {} tokens at {:.1} tok/s",
                            eval_count,
                            tokens_per_sec
                        );
                    }
                }
            }
        }
    }

    ensure!(
        !full_content.is_empty(),
        "Ollama returned empty response for prompt of {} chars",
        prompt.len()
    );

    Ok(full_content)
}
```

---

## 7. Non-Streaming Call Pattern (Simpler)

If you don't need streaming (wait for full response):

```rust
async fn query_ollama_sync(prompt: &str) -> Result<String> {
    let request = OllamaChatRequest {
        model: "qwen3.5-custom".to_string(),
        messages: vec![OllamaMessage {
            role: "user".to_string(),
            content: prompt.to_string(),
        }],
        stream: false, // Wait for complete response
        options: OllamaOptions { /* same as above */ },
    };

    let response: OllamaChatResponse = Client::new()
        .post(OLLAMA_URL)
        .json(&request)
        .send()
        .await
        .context("failed to connect to Ollama")?
        .json()
        .await
        .context("failed to parse Ollama response")?;

    ensure!(!response.message.content.is_empty(), "empty LLM response");
    Ok(response.message.content)
}
```

---

## 8. Prompt Design Notes

Since the model template is `{{ .Prompt }}` with **no system prompt**:

1. Everything goes in a single `"user"` role message
2. The prompt itself must contain all instructions
3. For JSON output, instruct explicitly in the prompt:

```
Below are the latest messages from Telegram news channels.
Focus ONLY on news from the last 15 minutes (from {cutoff_time} to {now}).

{channel_messages_formatted}

Respond with ONLY a JSON object in this exact format:
{
  "updates": [
    {
      "channel": "channel name",
      "headline": "brief headline",
      "importance": "high|medium|low",
      "summary": "1-2 sentence summary"
    }
  ],
  "meta": {
    "channels_analyzed": 5,
    "time_window_minutes": 15,
    "generated_at": "2026-03-03T12:00:00Z"
  }
}

If there are no notable updates, return {"updates": [], "meta": {...}}.
Output ONLY the JSON, no other text.
```

---

## 9. Performance Expectations (RTX 5090)

Based on the Modelfile and hardware:
- **Qwen 3.5 27B Q4_K_M** on RTX 5090 (32GB VRAM)
- **Prompt eval**: ~50k tokens of context will take significant time (estimate: 30-60s for prompt ingestion)
- **Generation**: The `num_predict: 81920` allows up to ~80K output tokens, but for brief JSON output, expect 200-500 tokens (~1-3s)
- **Total per iteration**: Dominated by prompt ingestion time. With 50k token context, expect ~30-90 seconds per LLM iteration depending on batch processing speed
- **Streaming** reduces perceived latency but not total time

---

## 10. Error Handling for Ollama

Common failure modes:
1. **Ollama not running**: `reqwest` connection refused → "failed to connect to Ollama"
2. **Model not loaded**: First request has high `load_duration` (cold start, model loaded from disk to VRAM)
3. **OOM**: If `num_ctx` is too large for available VRAM, Ollama will fail silently or crash
4. **Invalid model name**: Ollama returns HTTP 404
5. **Malformed JSON in streaming**: Partial chunks at connection boundaries — always buffer and split on newlines

All of these should be caught by the `anyhow` error chain in the patterns above.
