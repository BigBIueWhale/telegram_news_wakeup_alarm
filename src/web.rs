use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, RwLock};
use tower_http::services::ServeDir;
use tower_http::set_status::SetStatus;

// ── Shared state written by processor, read by HTTP handlers ──

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct WebUpdate {
    /// Monotonically increasing version — clients use this to detect changes.
    pub version: u64,
    /// ISO-8601 timestamp of when the displayed data was collected (snapshot time,
    /// before LLM generation). Tells the user how stale the displayed info is.
    pub data_collected_at: String,
    /// Threat assessment fields.
    /// Non-empty string = active threat (5-word reason why). Empty string = no threat.
    // Israel-wide (informational)
    pub israel_attack_warning: String,
    pub israel_actual_red_alerts: String,
    pub attack_involves_missiles_not_just_uavs: String,
    // Center / Jerusalem (alarm triggers)
    pub jerusalem_attack_warning: String,
    pub jerusalem_actual_red_alerts: String,
    pub center_dan_or_yehuda_or_jerusalem_danger: String,
    pub confirmed_center_attack_not_just_north_south: String,
    pub any_threat: bool,
    /// ISO-8601 timestamp of when the LLM started generating the current response.
    /// Empty string means the LLM is idle (not generating).
    pub generating_since: String,
    /// News items from the last LLM response.
    pub news: Vec<WebNewsItem>,
    /// Active channel buffer info (updated every snapshot, not just on LLM response).
    pub channels: Vec<WebChannelInfo>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct WebNewsItem {
    pub channel: String,
    pub headline: String,
    pub importance: String,
    pub time_of_report: String,
    pub time_of_event: String,
    pub summary: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct WebChannelInfo {
    pub name: String,
    pub message_count: usize,
    /// ISO-8601 timestamp of the newest message in this channel's buffer.
    pub latest_message: String,
}

pub type SharedWebState = Arc<RwLock<WebUpdate>>;

pub fn new_shared_state() -> SharedWebState {
    Arc::new(RwLock::new(WebUpdate::default()))
}

// ── Error response helper ──

pub fn error_json(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({
        "error": status.as_u16(),
        "status": status.canonical_reason().unwrap_or("Unknown"),
        "message": message,
    });
    (status, Json(body)).into_response()
}

// ── HTTP handlers ──

pub async fn api_status(State(state): State<SharedWebState>) -> Json<WebUpdate> {
    let data = state.read().expect("web state lock poisoned").clone();
    Json(data)
}

// ── Security middleware ──

/// Reject non-GET/HEAD methods with 405 and a clear message.
/// This server is read-only — no POST, PUT, DELETE, PATCH, etc.
async fn reject_non_get(req: Request<Body>, next: Next) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();

    if method != Method::GET && method != Method::HEAD {
        log::warn!(
            "rejected {} {} — only GET and HEAD are allowed",
            method, uri
        );
        return error_json(
            StatusCode::METHOD_NOT_ALLOWED,
            &format!(
                "method {} not allowed on this server — only GET and HEAD are supported. \
                 This is a read-only dashboard.",
                method
            ),
        );
    }

    next.run(req).await
}

/// Reject requests with a body (Content-Length > 0 or Transfer-Encoding).
/// GET/HEAD requests should never carry a body.
async fn reject_request_body(req: Request<Body>, next: Next) -> Response {
    let has_content_length = req
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|len| len > 0);

    let has_transfer_encoding = req.headers().contains_key("transfer-encoding");

    if has_content_length || has_transfer_encoding {
        log::warn!(
            "rejected {} {} — request has a body (GET/HEAD must not carry payloads)",
            req.method(), req.uri()
        );
        return error_json(
            StatusCode::PAYLOAD_TOO_LARGE,
            "this server does not accept request bodies — \
             all endpoints are read-only GET requests",
        );
    }

    next.run(req).await
}

/// Reject URIs that are suspiciously long (path traversal probes, fuzzing).
pub async fn reject_oversized_uri(req: Request<Body>, next: Next) -> Response {
    let uri_len = req.uri().to_string().len();
    if uri_len > 2048 {
        log::warn!(
            "rejected request with oversized URI ({} bytes, max 2048): {}…",
            uri_len,
            &req.uri().to_string()[..80]
        );
        return error_json(
            StatusCode::URI_TOO_LONG,
            &format!(
                "URI is {} bytes, maximum allowed is 2048 — \
                 this looks like a scanning probe or malformed request",
                uri_len
            ),
        );
    }

    next.run(req).await
}

/// Reject requests with too many headers (header stuffing attacks).
pub async fn reject_header_abuse(req: Request<Body>, next: Next) -> Response {
    let header_count = req.headers().len();
    if header_count > 64 {
        log::warn!(
            "rejected request with {} headers (max 64) to {} — possible header stuffing",
            header_count, req.uri()
        );
        return error_json(
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            &format!(
                "request has {} headers, maximum allowed is 64 — \
                 normal browsers send 5-15 headers",
                header_count
            ),
        );
    }

    // Reject any single header value > 8KB
    for (name, value) in req.headers() {
        if value.len() > 8192 {
            log::warn!(
                "rejected request with oversized header '{}' ({} bytes, max 8192)",
                name, value.len()
            );
            return error_json(
                StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
                &format!(
                    "header '{}' is {} bytes, maximum allowed per header is 8192",
                    name, value.len()
                ),
            );
        }
    }

    next.run(req).await
}

// ── Fallback handler for unmatched routes ──

pub async fn fallback_404(uri: Uri) -> Response {
    error_json(
        StatusCode::NOT_FOUND,
        &format!(
            "nothing at '{}' — the dashboard is at / and the API is at /api/status",
            uri.path()
        ),
    )
}

// ── Server entrypoint ──

/// Directory containing index.html, audio files, and any other static assets.
pub const WEB_DIR: &str = "web";

/// Static file serving with a 404 fallback for files not found.
/// Reusable by the simulator server (same frontend, different API).
pub fn static_files_fallback() -> anyhow::Result<ServeDir<SetStatus<axum::routing::MethodRouter>>> {
    let web_dir = Path::new(WEB_DIR);
    if !web_dir.is_dir() {
        anyhow::bail!(
            "web directory '{}' not found — it should contain index.html and any audio files",
            WEB_DIR
        );
    }
    Ok(ServeDir::new(WEB_DIR).not_found_service(axum::routing::get(fallback_404)))
}

pub async fn run_server(state: SharedWebState) -> anyhow::Result<()> {
    let static_files = static_files_fallback()?;

    // API routes take precedence; static files are the fallback.
    let app = axum::Router::new()
        .route("/api/status", get(api_status))
        .with_state(state)
        .fallback_service(static_files)
        // Security middleware stack (outermost runs first).
        .layer(middleware::from_fn(reject_header_abuse))
        .layer(middleware::from_fn(reject_oversized_uri))
        .layer(middleware::from_fn(reject_request_body))
        .layer(middleware::from_fn(reject_non_get));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:9876").await?;
    let web_path = Path::new(WEB_DIR);
    log::info!(
        "Web dashboard listening on http://0.0.0.0:9876 (serving from '{}')",
        web_path
            .canonicalize()
            .unwrap_or_else(|_| web_path.to_path_buf())
            .display()
    );

    axum::serve(listener, app).await?;
    Ok(())
}
