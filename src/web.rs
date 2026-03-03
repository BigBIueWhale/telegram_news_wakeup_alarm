use axum::extract::State;
use axum::routing::get;
use axum::Json;
use serde::Serialize;
use std::path::Path;
use std::sync::{Arc, RwLock};
use tower_http::services::ServeDir;

// ── Shared state written by processor, read by HTTP handlers ──

#[derive(Clone, Serialize, Default)]
pub struct WebUpdate {
    /// Monotonically increasing version — clients use this to detect changes.
    pub version: u64,
    /// ISO-8601 timestamp of last LLM update.
    pub timestamp: String,
    /// Threat assessment booleans.
    pub israel_attack_warning: bool,
    pub israel_actual_red_alerts: bool,
    pub jerusalem_attack_warning: bool,
    pub jerusalem_actual_red_alerts: bool,
    pub center_dan_or_or_yehuda_or_jerusalem_danger: bool,
    pub evidence_for_jerusalem_or_center_or_yehuda_not_just_north_or_south: bool,
    pub any_threat: bool,
    /// News items from the last LLM response.
    pub news: Vec<WebNewsItem>,
}

#[derive(Clone, Serialize)]
pub struct WebNewsItem {
    pub channel: String,
    pub headline: String,
    pub importance: String,
    pub summary: String,
}

pub type SharedWebState = Arc<RwLock<WebUpdate>>;

pub fn new_shared_state() -> SharedWebState {
    Arc::new(RwLock::new(WebUpdate::default()))
}

// ── HTTP handlers ──

async fn api_status(State(state): State<SharedWebState>) -> Json<WebUpdate> {
    let data = state.read().expect("web state lock poisoned").clone();
    Json(data)
}

// ── Server entrypoint ──

/// Directory containing index.html, audio files, and any other static assets.
const WEB_DIR: &str = "web";

pub async fn run_server(state: SharedWebState) -> anyhow::Result<()> {
    let web_dir = Path::new(WEB_DIR);
    if !web_dir.is_dir() {
        anyhow::bail!(
            "web directory '{}' not found — it should contain index.html and any audio files",
            WEB_DIR
        );
    }

    // API routes take precedence; everything else falls through to static files.
    let api = axum::Router::new()
        .route("/api/status", get(api_status))
        .with_state(state);

    let static_files = ServeDir::new(WEB_DIR);
    let app = api.fallback_service(static_files);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:9876").await?;
    log::info!(
        "Web dashboard listening on http://0.0.0.0:9876 (serving from '{}')",
        web_dir.canonicalize().unwrap_or_else(|_| web_dir.to_path_buf()).display()
    );
    axum::serve(listener, app).await?;
    Ok(())
}
