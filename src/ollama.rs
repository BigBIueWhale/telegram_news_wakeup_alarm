use anyhow::{ensure, Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

// ── Request types ──

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    think: bool,
    options: ChatOptions,
}

#[derive(Serialize, Deserialize, Clone)]
struct ChatMessage {
    role: String,
    content: String,
}

/// Model parameters matching the qwen3.5-custom Modelfile.
/// All fields are explicit — no implicit defaults from Ollama.
#[derive(Serialize)]
struct ChatOptions {
    num_ctx: u64,
    num_predict: u64,
    temperature: f32,
    top_k: u32,
    top_p: f32,
    presence_penalty: f32,
    repeat_penalty: f32,
}

// ── Response types ──

#[derive(Deserialize)]
struct ChatResponse {
    message: ChatMessage,
    done: bool,
    #[serde(default)]
    eval_count: Option<u64>,
    #[serde(default)]
    eval_duration: Option<u64>,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
}

/// Ollama LLM client using raw reqwest for full parameter control.
///
/// `ollama-rs` v0.3.4 does not support `presence_penalty` in its ModelOptions
/// struct (all fields are pub(super)), so we use raw HTTP requests to
/// `/api/chat` with the native Ollama JSON format.
pub struct OllamaClient {
    http: reqwest::Client,
    endpoint: String,
    model: String,
}

impl OllamaClient {
    pub fn new(base_url: String, model: String) -> Self {
        let endpoint = format!("{}/api/chat", base_url.trim_end_matches('/'));
        Self {
            http: reqwest::Client::new(),
            endpoint,
            model,
        }
    }

    /// Returns the configured model name (for logging).
    pub fn model_name(&self) -> &str {
        &self.model
    }

    /// Verify that Ollama is reachable and the configured model is available.
    /// Hits the `/api/tags` endpoint (lightweight, no GPU usage).
    /// Returns Ok(()) if reachable, Err with actionable message if not.
    pub async fn check_reachable(&self) -> Result<()> {
        let tags_url = self.endpoint.replace("/api/chat", "/api/tags");

        let response = self
            .http
            .get(&tags_url)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .with_context(|| {
                format!(
                    "cannot reach Ollama at '{}' — is Ollama running? \
                     (start it with: ollama serve)",
                    tags_url
                )
            })?;

        if !response.status().is_success() {
            anyhow::bail!(
                "Ollama returned HTTP {} from '{}' — expected 200. \
                 Is this a valid Ollama instance?",
                response.status(),
                tags_url
            );
        }

        #[derive(Deserialize)]
        struct TagsResponse {
            models: Vec<ModelInfo>,
        }
        #[derive(Deserialize)]
        struct ModelInfo {
            name: String,
        }

        let tags: TagsResponse = response.json().await.with_context(|| {
            format!("failed to parse Ollama /api/tags response from '{}'", tags_url)
        })?;

        let model_found = tags.models.iter().any(|m| {
            m.name == self.model || m.name.starts_with(&format!("{}:", self.model))
        });

        if !model_found {
            let available: Vec<&str> = tags.models.iter().map(|m| m.name.as_str()).collect();
            anyhow::bail!(
                "Ollama is running but model '{}' is not loaded. \
                 Available models: {:?}. Pull it with: ollama pull {}",
                self.model,
                available,
                self.model
            );
        }

        Ok(())
    }

    /// Send a prompt to the LLM and collect the full streamed response.
    ///
    /// Uses streaming mode for lower perceived latency and better error detection
    /// (we see partial output even if the connection drops mid-response).
    ///
    /// The prompt goes as a single "user" role message (no system prompt —
    /// the qwen3.5-custom Modelfile template is `{{ .Prompt }}`).
    pub async fn query(&self, prompt: &str) -> Result<String> {
        let request = ChatRequest {
            model: self.model.clone(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: prompt.to_string(),
            }],
            stream: true,
            think: false,
            options: ChatOptions {
                num_ctx: 131072,
                num_predict: 4096,
                temperature: 0.7,
                top_k: 20,
                top_p: 0.8,
                presence_penalty: 1.5,
                repeat_penalty: 1.0,
            },
        };

        let response = self
            .http
            .post(&self.endpoint)
            .json(&request)
            .send()
            .await
            .with_context(|| {
                format!(
                    "failed to connect to Ollama at '{}' — \
                     is Ollama running? (check with: curl {})",
                    self.endpoint, self.endpoint
                )
            })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_else(|_| "(unreadable body)".into());
            anyhow::bail!(
                "Ollama returned HTTP {} for model '{}' at '{}' — {}. \
                 If 404, the model may not be loaded (try: ollama pull {})",
                status,
                self.model,
                self.endpoint,
                &body[..500.min(body.len())],
                self.model
            );
        }

        // Stream newline-delimited JSON chunks and accumulate the full response content.
        let mut stream = response.bytes_stream();
        let mut full_content = String::new();
        let mut line_buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context(
                "Ollama response stream read error — the connection may have dropped mid-response",
            )?;
            line_buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete lines (Ollama streams one JSON object per line).
            while let Some(newline_pos) = line_buffer.find('\n') {
                let line: String = line_buffer[..newline_pos].trim().to_string();
                line_buffer = line_buffer[newline_pos + 1..].to_string();

                if line.is_empty() {
                    continue;
                }

                let resp: ChatResponse = serde_json::from_str(&line).with_context(|| {
                    format!(
                        "failed to parse Ollama streaming JSON chunk (model: '{}', line: '{}')",
                        self.model,
                        &line[..200.min(line.len())]
                    )
                })?;

                full_content.push_str(&resp.message.content);

                if resp.done {
                    if let (Some(eval_count), Some(eval_duration)) =
                        (resp.eval_count, resp.eval_duration)
                    {
                        if eval_duration > 0 {
                            let tok_per_sec =
                                eval_count as f64 / (eval_duration as f64 / 1_000_000_000.0);
                            log::info!(
                                "LLM: generated {} tokens at {:.1} tok/s (prompt ingested: {} tokens)",
                                eval_count,
                                tok_per_sec,
                                resp.prompt_eval_count.unwrap_or(0)
                            );
                        }
                    }
                }
            }
        }

        ensure!(
            !full_content.is_empty(),
            "Ollama returned an empty response for model '{}' — \
             the prompt was {} chars. The model may have hit a stop token immediately \
             or the context window ({} tokens) may be too small for the prompt",
            self.model,
            prompt.len(),
            131072
        );

        Ok(full_content)
    }
}
