use anyhow::{Context, Result};
use std::time::Duration;
use telegram_news_alarm::{buffer::ChannelBuffers, config::Config, ollama::OllamaClient, processor, telegram};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging (INFO level by default; set RUST_LOG for finer control).
    simple_logger::init_with_level(log::Level::Info)
        .context("failed to initialize logger — is another logger already registered?")?;

    // Load and validate all configuration from environment variables.
    let config = Config::from_env().context(
        "configuration error — set the required environment variables \
         (see .env.example) or export them in your shell",
    )?;

    log::info!("Loading Qwen 3.5 tokenizer from '{}'...", config.tokenizer_path);
    let tokenizer = tokenizers::Tokenizer::from_file(&config.tokenizer_path).map_err(|e| {
        anyhow::anyhow!(
            "failed to load tokenizer from '{}': {} — \
             download it with: curl -L https://huggingface.co/Qwen/Qwen3.5-27B/resolve/main/tokenizer.json -o {}",
            config.tokenizer_path,
            e,
            config.tokenizer_path
        )
    })?;
    log::info!(
        "Tokenizer loaded (vocab size: {})",
        tokenizer.get_vocab_size(false)
    );

    // Shared channel message buffers: written by Telegram listener, read by LLM processor.
    let buffers = ChannelBuffers::new();

    // Create the Ollama client (validates URL format, actual connectivity checked on first query).
    let ollama = OllamaClient::new(config.ollama_url.clone(), config.ollama_model.clone());
    log::info!(
        "Ollama client configured: model='{}', endpoint='{}/api/chat'",
        config.ollama_model,
        config.ollama_url
    );

    // Spawn the LLM processing loop as a background task.
    // It will wait for messages to arrive before its first query.
    let processor_buffers = buffers.clone();
    let processor_handle = tokio::spawn(async move {
        processor::run_loop(processor_buffers, tokenizer, ollama).await
    });

    // Run the Telegram listener with automatic reconnection on failure.
    // On Ctrl+C, run_listener returns Ok(()) and we break out to shut down.
    // On connection/protocol errors, we wait and retry with exponential backoff.
    let mut backoff = Duration::from_secs(5);
    loop {
        log::info!("Connecting to Telegram (session: '{}')...", config.session_path);
        match telegram::run_listener(&config, &buffers).await {
            Ok(()) => {
                // Graceful shutdown (Ctrl+C was pressed inside the listener).
                log::info!("Telegram listener shut down gracefully");
                break;
            }
            Err(e) => {
                // Fatal errors (bad credentials, banned account, revoked session)
                // should NOT be retried — they'll never succeed.
                if telegram::is_fatal_telegram_error(&e) {
                    log::error!("FATAL Telegram error (will NOT retry): {:#}", e);
                    return Err(e);
                }

                // Transient errors (network, timeout, server hiccup) get retried.
                log::error!(
                    "Telegram listener error (transient): {:#}\nReconnecting in {:?}...",
                    e,
                    backoff
                );
                tokio::time::sleep(backoff).await;
                // Exponential backoff: 5s, 10s, 20s, 40s, 60s (capped).
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }

    // Shut down the LLM processor task.
    log::info!("Stopping LLM processor...");
    processor_handle.abort();
    match processor_handle.await {
        Ok(Ok(())) => log::info!("LLM processor stopped cleanly"),
        Ok(Err(e)) => log::warn!("LLM processor reported error during shutdown: {:#}", e),
        Err(_) => log::debug!("LLM processor task cancelled (expected)"),
    }

    log::info!("Shutdown complete");
    Ok(())
}
