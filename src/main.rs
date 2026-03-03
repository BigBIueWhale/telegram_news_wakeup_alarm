use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Duration;
use telegram_news_alarm::{buffer::ChannelBuffers, config::Config, ollama::OllamaClient, processor, telegram};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging (INFO level by default; set RUST_LOG for finer control).
    simple_logger::SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .with_module_level("grammers_session::message_box", log::LevelFilter::Warn)
        .init()
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

    // Create the Ollama client and verify connectivity before starting anything else.
    let ollama = OllamaClient::new(config.ollama_url.clone(), config.ollama_model.clone());
    log::info!(
        "Checking Ollama connectivity (model='{}', endpoint='{}/api/chat')...",
        config.ollama_model,
        config.ollama_url
    );
    ollama.check_reachable().await.context(
        "Ollama reachability check failed at startup — \
         the LLM backend must be running before the application starts"
    )?;
    log::info!(
        "Ollama is reachable and model '{}' is available",
        config.ollama_model
    );

    // Shared shutdown signal: cancelled when the Telegram listener exits (Ctrl+C or fatal error).
    let shutdown = CancellationToken::new();

    // The processor must NOT start until Telegram authentication is complete,
    // otherwise its periodic log output interferes with the interactive login prompt.
    // run_listener signals the Notify after auth + peer cache are done.
    // The processor task is spawned once and blocks on the Notify before entering its loop.
    let telegram_ready = Arc::new(Notify::new());
    let processor_buffers = buffers.clone();
    let processor_shutdown = shutdown.clone();
    let processor_ready = Arc::clone(&telegram_ready);
    let processor_handle = tokio::spawn(async move {
        processor_ready.notified().await;
        log::info!("Telegram authenticated — starting LLM processor");
        processor::run_loop(processor_buffers, tokenizer, ollama, processor_shutdown).await
    });

    // Run the Telegram listener with automatic reconnection on failure.
    // On Ctrl+C, run_listener returns Ok(()) and we break out to shut down.
    // On connection/protocol errors, we wait and retry with exponential backoff.
    // The telegram_ready Notify is passed every time — it's harmless to signal it
    // more than once (subsequent notify_one() calls are no-ops after the first wake).
    loop {
        log::info!("Connecting to Telegram (session: '{}')...", config.session_path);
        match telegram::run_listener(&config, &buffers, Some(Arc::clone(&telegram_ready))).await {
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
                    processor_handle.abort();
                    return Err(e);
                }

                // Transient errors (network, timeout, server hiccup) get retried.
                // Always reconnect after 5s — if we got this far, auth succeeded
                // and updates were streaming. The error is a network blip, not
                // a cascading failure that needs exponential backoff.
                let reconnect_delay = Duration::from_secs(5);
                log::error!(
                    "Telegram listener error (transient): {:#}\nReconnecting in {:?}...",
                    e,
                    reconnect_delay
                );
                tokio::time::sleep(reconnect_delay).await;
            }
        }
    }

    // Gracefully shut down the LLM processor task.
    log::info!("Signaling LLM processor to shut down...");
    shutdown.cancel();
    let mut processor_handle = processor_handle;
    match tokio::time::timeout(Duration::from_secs(30), &mut processor_handle).await {
        Ok(Ok(Ok(()))) => log::info!("LLM processor stopped cleanly"),
        Ok(Ok(Err(e))) => log::warn!("LLM processor reported error during shutdown: {:#}", e),
        Ok(Err(e)) => log::warn!("LLM processor task panicked during shutdown: {:?}", e),
        Err(_) => {
            log::warn!("LLM processor did not stop within 30s — aborting");
            processor_handle.abort();
        }
    }

    log::info!("Shutdown complete");
    Ok(())
}
