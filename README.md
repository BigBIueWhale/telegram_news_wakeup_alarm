# Telegram News Wakeup Alarm

## Run

```bash
cargo build --release
set -a && source .env && set +a && ./target/release/news-alarm
```

## Dashboard

http://localhost:9876

## Files

| Path | Description |
|------|-------------|
| [.env](.env) | Environment variables (API keys, model config) |
| [src/main.rs](src/main.rs) | Entry point, task wiring, reconnect loop |
| [src/config.rs](src/config.rs) | Env var loading, channel allow-list |
| [src/telegram/mod.rs](src/telegram/mod.rs) | Telegram listener, history backfill |
| [src/telegram/auth.rs](src/telegram/auth.rs) | Interactive login (phone code + 2FA) |
| [src/buffer.rs](src/buffer.rs) | Per-channel circular message buffers |
| [src/processor.rs](src/processor.rs) | LLM prompt building, token budgeting, output parsing |
| [src/ollama.rs](src/ollama.rs) | Ollama HTTP client (streaming) |
| [src/web.rs](src/web.rs) | HTTP server, shared state |
| [web/index.html](web/index.html) | Dashboard UI, polling engine, sound logic |
| [web/alarm.wav](web/alarm.wav) | Alert sound |
