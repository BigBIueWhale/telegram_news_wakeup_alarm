use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use std::path::Path;
use std::sync::{Arc, RwLock};
use telegram_news_alarm::web::{
    self, SharedWebState, WebChannelInfo, WebNewsItem, WebUpdate,
    error_json, reject_header_abuse, reject_oversized_uri, WEB_DIR,
};

// ── Simulator state ──

/// Simulator-specific wrapper: same WebUpdate, but behind our own Arc<RwLock>
/// so mutations go through the control API.
struct SimState {
    web: SharedWebState,
}

impl SimState {
    fn new() -> Self {
        let initial = WebUpdate {
            version: 1,
            data_collected_at: now_iso(),
            channels: vec![
                WebChannelInfo {
                    name: "Sim: Breaking News".into(),
                    message_count: 42,
                    latest_message: now_iso(),
                },
                WebChannelInfo {
                    name: "Sim: Alerts Channel".into(),
                    message_count: 18,
                    latest_message: now_iso(),
                },
            ],
            ..WebUpdate::default()
        };
        Self {
            web: Arc::new(RwLock::new(initial)),
        }
    }

    fn shared(&self) -> SharedWebState {
        self.web.clone()
    }
}

// ── Scenario definitions ──

#[derive(Deserialize)]
struct ScenarioRequest {
    scenario: String,
}

fn apply_scenario(state: &SharedWebState, name: &str) -> Result<(), String> {
    let mut data = state.write().expect("web state lock poisoned");
    match name {
        "alarm" => {
            data.israel_attack_warning = "Missile salvo from Iran detected".into();
            data.israel_actual_red_alerts = "Red alerts across central Israel".into();
            data.attack_involves_missiles_not_just_uavs =
                "Ballistic missiles confirmed, not UAVs".into();
            data.jerusalem_attack_warning = "Jerusalem area under direct threat".into();
            data.jerusalem_actual_red_alerts = "Red alert active in Jerusalem".into();
            data.center_dan_or_yehuda_or_jerusalem_danger =
                "Dan region and Jerusalem endangered".into();
            data.confirmed_center_attack_not_just_north_south =
                "Center attack confirmed, not periphery".into();
            data.any_threat = true;
            data.news = vec![WebNewsItem {
                channel: "Sim: Breaking News".into(),
                headline: "BREAKING: Large-scale missile attack on central Israel".into(),
                importance: "critical".into(),
                time_of_report: now_iso(),
                time_of_event: now_iso(),
                summary: "Multiple ballistic missiles launched toward Jerusalem and Tel Aviv. \
                          Sirens sounding across the center. Seek shelter immediately."
                    .into(),
            }];
        }
        "tick" => {
            data.israel_attack_warning = "UAV swarm detected heading south".into();
            data.israel_actual_red_alerts = "Red alerts in northern regions".into();
            data.attack_involves_missiles_not_just_uavs = String::new();
            data.jerusalem_attack_warning = String::new();
            data.jerusalem_actual_red_alerts = String::new();
            data.center_dan_or_yehuda_or_jerusalem_danger = String::new();
            data.confirmed_center_attack_not_just_north_south = String::new();
            data.any_threat = true;
            data.news = vec![WebNewsItem {
                channel: "Sim: Alerts Channel".into(),
                headline: "UAV threat detected in northern airspace".into(),
                importance: "high".into(),
                time_of_report: now_iso(),
                time_of_event: now_iso(),
                summary: "Multiple UAVs detected. Northern communities alerted. \
                          No immediate threat to center but situation developing."
                    .into(),
            }];
        }
        "calm" => {
            data.israel_attack_warning = String::new();
            data.israel_actual_red_alerts = String::new();
            data.attack_involves_missiles_not_just_uavs = String::new();
            data.jerusalem_attack_warning = String::new();
            data.jerusalem_actual_red_alerts = String::new();
            data.center_dan_or_yehuda_or_jerusalem_danger = String::new();
            data.confirmed_center_attack_not_just_north_south = String::new();
            data.any_threat = false;
            data.news = vec![
                WebNewsItem {
                    channel: "כאן חדשות".into(),
                    headline: "תחזית מזג אוויר: חם מהרגיל לעונה, עד 28 מעלות במרכז".into(),
                    importance: "low".into(),
                    time_of_report: now_iso(),
                    time_of_event: now_iso(),
                    summary: "Unseasonably warm weather expected across central Israel today. \
                              Temperatures reaching 28°C. Light winds, no rain expected."
                        .into(),
                },
                WebNewsItem {
                    channel: "חדשות הארץ".into(),
                    headline: "עיריית תל אביב מרחיבה את שבילי האופניים ברחוב דיזנגוף".into(),
                    importance: "low".into(),
                    time_of_report: now_iso(),
                    time_of_event: now_iso(),
                    summary: "Tel Aviv municipality expanding bike lanes on Dizengoff Street. \
                              Construction expected to cause minor traffic disruptions this week."
                        .into(),
                },
                WebNewsItem {
                    channel: "ynet updates".into(),
                    headline: "Knesset committee approves budget amendment for school renovations".into(),
                    importance: "medium".into(),
                    time_of_report: now_iso(),
                    time_of_event: now_iso(),
                    summary: "NIS 500M allocated for school building renovations nationwide. \
                              Priority given to earthquake-proofing older structures in the north."
                        .into(),
                },
            ];
        }
        "clear" => {
            data.israel_attack_warning = String::new();
            data.israel_actual_red_alerts = String::new();
            data.attack_involves_missiles_not_just_uavs = String::new();
            data.jerusalem_attack_warning = String::new();
            data.jerusalem_actual_red_alerts = String::new();
            data.center_dan_or_yehuda_or_jerusalem_danger = String::new();
            data.confirmed_center_attack_not_just_north_south = String::new();
            data.any_threat = false;
            data.news = vec![];
        }
        "missiles" => {
            data.israel_attack_warning = "Massive missile barrage from multiple fronts".into();
            data.israel_actual_red_alerts = "Nationwide red alert — all regions".into();
            data.attack_involves_missiles_not_just_uavs =
                "Ballistic and cruise missiles confirmed".into();
            data.jerusalem_attack_warning =
                "Jerusalem under imminent ballistic missile threat".into();
            data.jerusalem_actual_red_alerts = "Multiple impacts reported near Jerusalem".into();
            data.center_dan_or_yehuda_or_jerusalem_danger =
                "Full center region under active attack".into();
            data.confirmed_center_attack_not_just_north_south =
                "Confirmed multi-front center attack".into();
            data.any_threat = true;
            data.news = vec![
                WebNewsItem {
                    channel: "Sim: Breaking News".into(),
                    headline: "URGENT: Massive multi-front missile attack underway".into(),
                    importance: "critical".into(),
                    time_of_report: now_iso(),
                    time_of_event: now_iso(),
                    summary: "Coordinated missile barrage from Iran and Hezbollah. \
                              Hundreds of projectiles detected. Iron Dome and Arrow active."
                        .into(),
                },
                WebNewsItem {
                    channel: "Sim: Alerts Channel".into(),
                    headline: "IDF confirms interceptions over Jerusalem and Tel Aviv".into(),
                    importance: "critical".into(),
                    time_of_report: now_iso(),
                    time_of_event: now_iso(),
                    summary: "Arrow system intercepting ballistic missiles at high altitude. \
                              Iron Dome engaging lower-altitude threats. Stay in shelters."
                        .into(),
                },
                WebNewsItem {
                    channel: "Sim: Breaking News".into(),
                    headline: "Impacts reported in multiple locations".into(),
                    importance: "critical".into(),
                    time_of_report: now_iso(),
                    time_of_event: now_iso(),
                    summary: "Some missiles penetrated defenses. Emergency services responding. \
                              Casualties feared. Do not leave shelter until all-clear."
                        .into(),
                },
            ];
        }
        other => return Err(format!("unknown scenario '{}' — use alarm, tick, clear, calm, or missiles", other)),
    }
    data.version += 1;
    data.data_collected_at = now_iso();
    Ok(())
}

// ── Partial field update ──

/// All settable threat fields. Validated by the CLI and server.
pub const THREAT_FIELDS: &[&str] = &[
    "israel_attack_warning",
    "israel_actual_red_alerts",
    "attack_involves_missiles_not_just_uavs",
    "jerusalem_attack_warning",
    "jerusalem_actual_red_alerts",
    "center_dan_or_yehuda_or_jerusalem_danger",
    "confirmed_center_attack_not_just_north_south",
];

#[derive(Deserialize)]
struct SetFieldRequest {
    field: String,
    value: String,
}

fn set_field(state: &SharedWebState, field: &str, value: &str) -> Result<(), String> {
    let mut data = state.write().expect("web state lock poisoned");
    match field {
        "israel_attack_warning" => data.israel_attack_warning = value.to_owned(),
        "israel_actual_red_alerts" => data.israel_actual_red_alerts = value.to_owned(),
        "attack_involves_missiles_not_just_uavs" => {
            data.attack_involves_missiles_not_just_uavs = value.to_owned()
        }
        "jerusalem_attack_warning" => data.jerusalem_attack_warning = value.to_owned(),
        "jerusalem_actual_red_alerts" => data.jerusalem_actual_red_alerts = value.to_owned(),
        "center_dan_or_yehuda_or_jerusalem_danger" => {
            data.center_dan_or_yehuda_or_jerusalem_danger = value.to_owned()
        }
        "confirmed_center_attack_not_just_north_south" => {
            data.confirmed_center_attack_not_just_north_south = value.to_owned()
        }
        other => {
            return Err(format!(
                "unknown field '{}' — valid fields: {}",
                other,
                THREAT_FIELDS.join(", ")
            ))
        }
    }
    // Recompute any_threat: true if any threat field is non-empty.
    data.any_threat = !data.israel_attack_warning.is_empty()
        || !data.israel_actual_red_alerts.is_empty()
        || !data.attack_involves_missiles_not_just_uavs.is_empty()
        || !data.jerusalem_attack_warning.is_empty()
        || !data.jerusalem_actual_red_alerts.is_empty()
        || !data.center_dan_or_yehuda_or_jerusalem_danger.is_empty()
        || !data.confirmed_center_attack_not_just_north_south.is_empty();
    data.version += 1;
    data.data_collected_at = now_iso();
    Ok(())
}

// ── HTTP handlers ──

async fn sim_scenario(
    State(state): State<SharedWebState>,
    Json(req): Json<ScenarioRequest>,
) -> Response {
    match apply_scenario(&state, &req.scenario) {
        Ok(()) => {
            let v = state.read().expect("web state lock poisoned").version;
            Json(serde_json::json!({
                "ok": true,
                "scenario": req.scenario,
                "version": v,
            }))
            .into_response()
        }
        Err(msg) => error_json(StatusCode::BAD_REQUEST, &msg),
    }
}

async fn sim_set(
    State(state): State<SharedWebState>,
    Json(req): Json<SetFieldRequest>,
) -> Response {
    match set_field(&state, &req.field, &req.value) {
        Ok(()) => {
            let v = state.read().expect("web state lock poisoned").version;
            Json(serde_json::json!({
                "ok": true,
                "field": req.field,
                "value": req.value,
                "version": v,
            }))
            .into_response()
        }
        Err(msg) => error_json(StatusCode::BAD_REQUEST, &msg),
    }
}

async fn sim_state(State(state): State<SharedWebState>) -> Json<WebUpdate> {
    web::api_status(State(state)).await
}

/// Reject requests with Content-Length > 64KB on POST endpoints.
/// Prevents abuse while allowing normal JSON payloads.
async fn reject_oversized_body(req: Request<Body>, next: Next) -> Response {
    let content_length = req
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    if content_length > 65_536 {
        return error_json(
            StatusCode::PAYLOAD_TOO_LARGE,
            &format!(
                "request body is {} bytes, maximum allowed is 65536",
                content_length
            ),
        );
    }

    next.run(req).await
}

// ── Utilities ──

fn now_iso() -> String {
    chrono::Utc::now()
        .with_timezone(&chrono_tz::Asia::Jerusalem)
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

// ── Server entrypoint ──
//
// Two listeners for security (this PC is in a router DMZ):
//   0.0.0.0:9876   — public-facing read-only frontend (static files + /api/status)
//                     Same security posture as the production server.
//   127.0.0.1:9877 — localhost-only control API (/api/sim/*)
//                     Never exposed to the network.

/// Public-facing port — serves the same frontend + /api/status as production.
const PUBLIC_ADDR: &str = "0.0.0.0:9876";
/// Localhost-only port — simulator control API.
const CONTROL_ADDR: &str = "127.0.0.1:9877";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    simple_logger::SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .init()
        .expect("failed to initialize logger");

    let web_dir = Path::new(WEB_DIR);
    if !web_dir.is_dir() {
        anyhow::bail!(
            "web directory '{}' not found — run from the project root \
             (it should contain index.html and audio files)",
            WEB_DIR
        );
    }

    let sim = SimState::new();
    let state = sim.shared();

    // ── Public router (0.0.0.0:9876) ──
    // Read-only: static files + /api/status. Full production security middleware.
    let static_files = web::static_files_fallback()?;
    let public_app = axum::Router::new()
        .route("/api/status", get(web::api_status))
        .with_state(state.clone())
        .fallback_service(static_files)
        .layer(middleware::from_fn(reject_header_abuse))
        .layer(middleware::from_fn(reject_oversized_uri));

    // ── Control router (127.0.0.1:9877) ──
    // Localhost-only: mutable sim endpoints + state view.
    let control_app = axum::Router::new()
        .route("/api/sim/scenario", post(sim_scenario))
        .route("/api/sim/set", post(sim_set))
        .route("/api/sim/state", get(sim_state))
        .with_state(state)
        .layer(middleware::from_fn(reject_header_abuse))
        .layer(middleware::from_fn(reject_oversized_uri))
        .layer(middleware::from_fn(reject_oversized_body));

    let public_listener = tokio::net::TcpListener::bind(PUBLIC_ADDR).await?;
    let control_listener = tokio::net::TcpListener::bind(CONTROL_ADDR).await?;

    log::info!(
        "Simulator frontend on http://{} (serving from '{}')",
        PUBLIC_ADDR,
        web_dir
            .canonicalize()
            .unwrap_or_else(|_| web_dir.to_path_buf())
            .display()
    );
    log::info!("Simulator control API on http://{} (localhost only)", CONTROL_ADDR);
    log::info!("  POST /api/sim/scenario  — apply preset (alarm, tick, clear, missiles)");
    log::info!("  POST /api/sim/set       — set individual threat field");
    log::info!("  GET  /api/sim/state     — view current state");

    // Run both servers concurrently; exit if either fails.
    tokio::try_join!(
        async { axum::serve(public_listener, public_app).await.map_err(anyhow::Error::from) },
        async { axum::serve(control_listener, control_app).await.map_err(anyhow::Error::from) },
    )?;

    Ok(())
}
