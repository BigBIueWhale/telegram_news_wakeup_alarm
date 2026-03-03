use anyhow::{bail, Context, Result};
use std::path::Path;

/// All configuration for the application, loaded exclusively from environment variables.
/// Every field is validated eagerly at startup — no silent fallbacks, no latent failures.
/// If anything is wrong, we bail immediately with a specific, actionable error message.
pub struct Config {
    pub api_id: i32,
    pub api_hash: String,
    pub phone: String,
    pub session_path: String,
    pub ollama_url: String,
    pub ollama_model: String,
    pub tokenizer_path: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        // ── TELEGRAM_API_ID (required, positive integer) ──

        let api_id_str = std::env::var("TELEGRAM_API_ID")
            .context("TELEGRAM_API_ID environment variable not set — get yours from https://my.telegram.org")?;
        let api_id_str = api_id_str.trim();
        if api_id_str.is_empty() {
            bail!("TELEGRAM_API_ID is set but empty — it should be a numeric ID from my.telegram.org");
        }
        let api_id: i32 = api_id_str.parse().with_context(|| {
            format!(
                "TELEGRAM_API_ID '{}' is not a valid 32-bit integer — \
                 expected a numeric ID like 32917249 from my.telegram.org",
                api_id_str
            )
        })?;
        if api_id <= 0 {
            bail!(
                "TELEGRAM_API_ID is {} but must be a positive integer — \
                 check my.telegram.org for the correct value",
                api_id
            );
        }

        // ── TELEGRAM_API_HASH (required, exactly 32 lowercase hex characters) ──

        let api_hash = std::env::var("TELEGRAM_API_HASH")
            .context("TELEGRAM_API_HASH environment variable not set — get yours from https://my.telegram.org")?;
        let api_hash = api_hash.trim().to_string();
        if api_hash.is_empty() {
            bail!("TELEGRAM_API_HASH is set but empty — it should be a 32-character hex string from my.telegram.org");
        }
        if api_hash.len() != 32 {
            bail!(
                "TELEGRAM_API_HASH has {} characters but must be exactly 32 hex characters — \
                 check that you copied the full hash from my.telegram.org (got: '{}...')",
                api_hash.len(),
                &api_hash[..api_hash.len().min(8)]
            );
        }
        if !api_hash.chars().all(|c| c.is_ascii_hexdigit()) {
            let bad_chars: Vec<char> = api_hash
                .chars()
                .filter(|c| !c.is_ascii_hexdigit())
                .collect();
            bail!(
                "TELEGRAM_API_HASH contains non-hex characters {:?} — \
                 it should be exactly 32 characters using only 0-9 and a-f. \
                 Check my.telegram.org for the correct value",
                bad_chars
            );
        }

        // ── TELEGRAM_PHONE (required, international format: + then digits) ──

        let phone = std::env::var("TELEGRAM_PHONE")
            .context("TELEGRAM_PHONE environment variable not set — use international format like +972545552665")?;
        let phone = phone.trim().to_string();
        if phone.is_empty() {
            bail!("TELEGRAM_PHONE is set but empty — use international format like +972545552665");
        }
        if !phone.starts_with('+') {
            bail!(
                "TELEGRAM_PHONE '{}' must start with '+' followed by country code (e.g. +972545552665)",
                phone
            );
        }
        if phone.len() < 8 {
            bail!(
                "TELEGRAM_PHONE '{}' is too short ({} chars) — \
                 international phone numbers are typically 8-15 digits including country code",
                phone,
                phone.len()
            );
        }
        if phone.len() > 16 {
            bail!(
                "TELEGRAM_PHONE '{}' is too long ({} chars) — \
                 international phone numbers are at most 15 digits plus the leading '+'",
                phone,
                phone.len()
            );
        }
        if !phone[1..].chars().all(|c| c.is_ascii_digit()) {
            let bad_chars: Vec<char> = phone[1..]
                .chars()
                .filter(|c| !c.is_ascii_digit())
                .collect();
            bail!(
                "TELEGRAM_PHONE '{}' contains non-digit characters {:?} after the '+' — \
                 only digits are allowed (e.g. +972545552665)",
                phone,
                bad_chars
            );
        }

        // ── TELEGRAM_SESSION_PATH (optional, defaults to "telegram.session") ──

        let session_path = std::env::var("TELEGRAM_SESSION_PATH")
            .unwrap_or_else(|_| "telegram.session".to_string());
        let session_path = session_path.trim().to_string();
        if session_path.is_empty() {
            bail!("TELEGRAM_SESSION_PATH is set but empty — either unset it (default: telegram.session) or provide a valid path");
        }
        // Validate the parent directory exists (the session file itself may not exist yet — that's fine).
        if let Some(parent) = Path::new(&session_path).parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                bail!(
                    "TELEGRAM_SESSION_PATH '{}' points to a non-existent directory '{}' — \
                     create the directory first or use a path in an existing directory",
                    session_path,
                    parent.display()
                );
            }
        }

        // ── OLLAMA_URL (optional, defaults to "http://172.17.0.1:11434") ──

        let ollama_url = std::env::var("OLLAMA_URL")
            .unwrap_or_else(|_| "http://172.17.0.1:11434".to_string());
        let ollama_url = ollama_url.trim().trim_end_matches('/').to_string();
        if ollama_url.is_empty() {
            bail!("OLLAMA_URL is set but empty — either unset it (default: http://172.17.0.1:11434) or provide a valid URL");
        }
        if !ollama_url.starts_with("http://") && !ollama_url.starts_with("https://") {
            bail!(
                "OLLAMA_URL '{}' must start with http:// or https:// — \
                 expected something like http://172.17.0.1:11434",
                ollama_url
            );
        }
        // Basic structure check: after the protocol, there should be a host
        let after_protocol = if ollama_url.starts_with("https://") {
            &ollama_url[8..]
        } else {
            &ollama_url[7..]
        };
        if after_protocol.is_empty() || after_protocol.starts_with('/') {
            bail!(
                "OLLAMA_URL '{}' has no hostname — expected something like http://172.17.0.1:11434",
                ollama_url
            );
        }

        // ── OLLAMA_MODEL (optional, defaults to "qwen3.5-custom") ──

        let ollama_model = std::env::var("OLLAMA_MODEL")
            .unwrap_or_else(|_| "qwen3.5-custom".to_string());
        let ollama_model = ollama_model.trim().to_string();
        if ollama_model.is_empty() {
            bail!(
                "OLLAMA_MODEL is set but empty — either unset it (default: qwen3.5-custom) \
                 or provide a model name (e.g. qwen3.5-custom, llama3, etc.)"
            );
        }

        // ── TOKENIZER_PATH (optional, defaults to "tokenizer.json") ──

        let tokenizer_path = std::env::var("TOKENIZER_PATH")
            .unwrap_or_else(|_| "tokenizer.json".to_string());
        let tokenizer_path = tokenizer_path.trim().to_string();
        if tokenizer_path.is_empty() {
            bail!("TOKENIZER_PATH is set but empty — either unset it (default: tokenizer.json) or provide a valid file path");
        }
        if !Path::new(&tokenizer_path).exists() {
            bail!(
                "tokenizer file not found at '{}' — download it with:\n  \
                 curl -L https://huggingface.co/Qwen/Qwen3.5-27B/resolve/main/tokenizer.json -o {}",
                tokenizer_path,
                tokenizer_path
            );
        }
        if Path::new(&tokenizer_path).is_dir() {
            bail!(
                "TOKENIZER_PATH '{}' is a directory, not a file — \
                 it should point to the tokenizer.json file itself",
                tokenizer_path
            );
        }

        Ok(Config {
            api_id,
            api_hash,
            phone,
            session_path,
            ollama_url,
            ollama_model,
            tokenizer_path,
        })
    }
}
