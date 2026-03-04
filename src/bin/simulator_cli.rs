use clap::{Parser, Subcommand};

/// CLI control for the news-alarm-simulator server.
///
/// Sends commands to a running simulator instance via its HTTP control API.
#[derive(Parser)]
#[command(name = "news-alarm-simulator-cli", version, about)]
struct Cli {
    /// Base URL of the simulator control API (localhost-only by design).
    #[arg(long, default_value = "http://127.0.0.1:9877")]
    url: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Trigger Jerusalem alarm scenario (shorthand for `scenario alarm`).
    Alarm,
    /// Trigger general threat / tick scenario (shorthand for `scenario tick`).
    Tick,
    /// Clear all threats (shorthand for `scenario clear`).
    Clear,
    /// Calm mode with mundane news, no threats (shorthand for `scenario calm`).
    Calm,
    /// Apply a named scenario.
    Scenario {
        /// Scenario name: alarm, tick, clear, or missiles.
        name: String,
    },
    /// Set an individual threat field (empty string to clear).
    Set {
        /// Field name (e.g. jerusalem_attack_warning).
        field: String,
        /// Value to set (use "" to clear).
        value: String,
    },
    /// Print the current simulator state as JSON.
    State,
}

/// Valid threat field names — validated client-side before sending to server.
const VALID_FIELDS: &[&str] = &[
    "israel_attack_warning",
    "israel_actual_red_alerts",
    "attack_involves_missiles_not_just_uavs",
    "jerusalem_attack_warning",
    "jerusalem_actual_red_alerts",
    "center_dan_or_yehuda_or_jerusalem_danger",
    "confirmed_center_attack_not_just_north_south",
];

const VALID_SCENARIOS: &[&str] = &["alarm", "tick", "clear", "calm", "missiles"];

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let base = cli.url.trim_end_matches('/');

    let result = match cli.command {
        Command::Alarm => post_scenario(base, "alarm").await,
        Command::Tick => post_scenario(base, "tick").await,
        Command::Clear => post_scenario(base, "clear").await,
        Command::Calm => post_scenario(base, "calm").await,
        Command::Scenario { ref name } => {
            if !VALID_SCENARIOS.contains(&name.as_str()) {
                eprintln!(
                    "error: unknown scenario '{}' — valid: {}",
                    name,
                    VALID_SCENARIOS.join(", ")
                );
                std::process::exit(1);
            }
            post_scenario(base, name).await
        }
        Command::Set {
            ref field,
            ref value,
        } => {
            if !VALID_FIELDS.contains(&field.as_str()) {
                eprintln!(
                    "error: unknown field '{}' — valid fields:\n  {}",
                    field,
                    VALID_FIELDS.join("\n  ")
                );
                std::process::exit(1);
            }
            post_set(base, field, value).await
        }
        Command::State => get_state(base).await,
    };

    if let Err(e) = result {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}

async fn post_scenario(base: &str, scenario: &str) -> Result<(), String> {
    let url = format!("{}/api/sim/scenario", base);
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "scenario": scenario }))
        .send()
        .await
        .map_err(|e| format!("failed to connect to simulator at {}: {}", url, e))?;

    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid response from simulator: {}", e))?;

    if status.is_success() {
        println!(
            "scenario '{}' applied (version {})",
            scenario,
            body.get("version").and_then(|v| v.as_u64()).unwrap_or(0)
        );
        Ok(())
    } else {
        Err(format!(
            "simulator returned {}: {}",
            status,
            body.get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
        ))
    }
}

async fn post_set(base: &str, field: &str, value: &str) -> Result<(), String> {
    let url = format!("{}/api/sim/set", base);
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "field": field, "value": value }))
        .send()
        .await
        .map_err(|e| format!("failed to connect to simulator at {}: {}", url, e))?;

    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid response from simulator: {}", e))?;

    if status.is_success() {
        if value.is_empty() {
            println!(
                "field '{}' cleared (version {})",
                field,
                body.get("version").and_then(|v| v.as_u64()).unwrap_or(0)
            );
        } else {
            println!(
                "field '{}' set to '{}' (version {})",
                field,
                value,
                body.get("version").and_then(|v| v.as_u64()).unwrap_or(0)
            );
        }
        Ok(())
    } else {
        Err(format!(
            "simulator returned {}: {}",
            status,
            body.get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
        ))
    }
}

async fn get_state(base: &str) -> Result<(), String> {
    let url = format!("{}/api/sim/state", base);
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| format!("failed to connect to simulator at {}: {}", url, e))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("simulator returned {}", status));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid response from simulator: {}", e))?;

    // Pretty-print the state.
    println!(
        "{}",
        serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string())
    );
    Ok(())
}
