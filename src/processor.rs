use crate::buffer::{ChannelMessage, ChannelBuffers, ChannelSnapshot};
use crate::ollama::OllamaClient;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Deserialize;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::time::{Duration, Instant};
use tokenizers::Tokenizer;
use tokio_util::sync::CancellationToken;

/// Maximum token budget for the LLM prompt (must stay strictly LESS than this).
const MAX_TOKENS: usize = 50_000;

/// The LLM is instructed to focus analysis on this many minutes of recent activity.
/// Context older than this is provided only for background understanding.
const FOCUS_MINUTES: i64 = 15;

/// Don't bother querying the LLM if there are fewer messages than this.
const MIN_MESSAGES_TO_PROCESS: usize = 1;

// ── LLM output types (for JSON parsing validation) ──

#[derive(Deserialize, Debug)]
struct NewsOutput {
    updates: Vec<NewsItem>,
}

#[derive(Deserialize, Debug)]
struct NewsItem {
    channel: String,
    headline: String,
    importance: String,
    summary: String,
}

/// Format a Duration into a human-readable string.
/// Examples: "0.3s", "2.3s", "42s", "1m 12s"
fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let millis = d.subsec_millis();

    if total_secs == 0 {
        return format!("0.{}s", millis / 100);
    }
    if total_secs < 10 {
        return format!("{}.{}s", total_secs, millis / 100);
    }
    if total_secs < 60 {
        return format!("{}s", total_secs);
    }
    let minutes = total_secs / 60;
    let secs = total_secs % 60;
    format!("{}m {}s", minutes, secs)
}

/// Maximum number of iteration timestamps to track for rolling average.
const FREQUENCY_WINDOW: usize = 10;

/// The main LLM processing loop. Runs until the shutdown token is cancelled:
///   1. Snapshot all channel buffers
///   2. Binary-search for the optimal time window that maximizes context under 50k tokens
///   3. Build the prompt with uniform per-channel history
///   4. Query the LLM via Ollama
///   5. Parse the JSON response and print a human-readable summary to stdout
///   6. Immediately repeat (no artificial delay — throughput is LLM-bound)
pub async fn run_loop(
    buffers: ChannelBuffers,
    tokenizer: Tokenizer,
    ollama: OllamaClient,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut consecutive_errors: u32 = 0;
    // Track timestamps of productive iterations for rolling average interval.
    let mut iteration_times: VecDeque<Instant> = VecDeque::with_capacity(FREQUENCY_WINDOW + 1);

    loop {
        if shutdown.is_cancelled() {
            log::info!("[processor] Shutdown signal received — exiting");
            return Ok(());
        }

        // Step 1: Snapshot buffers
        log::info!("[processor] Snapshotting channel buffers...");
        let step_start = Instant::now();
        let snapshot = buffers.snapshot();
        log::info!(
            "[processor] Snapshot complete ({}) — {} channels",
            format_duration(step_start.elapsed()),
            snapshot.len()
        );

        // Wait if no messages have arrived yet.
        if snapshot.is_empty() {
            log::info!("[processor] No messages yet — waiting 5s...");
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                _ = shutdown.cancelled() => {
                    log::info!("[processor] Shutdown signal received — exiting");
                    return Ok(());
                }
            }
            continue;
        }

        let total_msgs: usize = snapshot.iter().map(|s| s.messages.len()).sum();
        if total_msgs < MIN_MESSAGES_TO_PROCESS {
            log::info!(
                "[processor] Only {} messages (need {}) — waiting 5s...",
                total_msgs, MIN_MESSAGES_TO_PROCESS
            );
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                _ = shutdown.cancelled() => {
                    log::info!("[processor] Shutdown signal received — exiting");
                    return Ok(());
                }
            }
            continue;
        }

        // Track iteration frequency (only for productive iterations).
        let now_instant = Instant::now();
        iteration_times.push_back(now_instant);
        if iteration_times.len() > FREQUENCY_WINDOW + 1 {
            iteration_times.pop_front();
        }
        if iteration_times.len() >= 2 {
            let first = *iteration_times.front().unwrap();
            let last = *iteration_times.back().unwrap();
            let total_elapsed = last.duration_since(first);
            let intervals = (iteration_times.len() - 1) as u32;
            let avg = total_elapsed / intervals;
            log::info!(
                "[processor] Average update interval: {} (over last {} runs)",
                format_duration(avg),
                intervals
            );
        }

        // Step 2: Build token-budgeted prompt
        log::info!(
            "[processor] Building token-budgeted prompt ({} channels, {} messages)...",
            snapshot.len(),
            total_msgs
        );
        let step_start = Instant::now();

        match build_budgeted_prompt(&snapshot, &tokenizer) {
            Ok((prompt, window_minutes, token_count)) => {
                log::info!(
                    "[processor] Prompt built ({}): {}min window, ~{} tokens",
                    format_duration(step_start.elapsed()),
                    window_minutes,
                    token_count,
                );

                // Step 3: Query LLM
                log::info!(
                    "[processor] Querying Ollama ('{}')...",
                    ollama.model_name()
                );
                let step_start = Instant::now();

                let query_result = tokio::select! {
                    result = ollama.query(&prompt) => Some(result),
                    _ = shutdown.cancelled() => None,
                };

                match query_result {
                    None => {
                        log::info!("[processor] Shutdown signal received during LLM query — exiting");
                        return Ok(());
                    }
                    Some(Ok(response)) => {
                        log::info!(
                            "[processor] LLM response received ({})",
                            format_duration(step_start.elapsed())
                        );
                        consecutive_errors = 0;

                        // Step 4: Parse and display output
                        log::info!("[processor] Parsing LLM response...");
                        let step_start = Instant::now();
                        process_and_print_output(&response);
                        log::info!(
                            "[processor] Output processed ({})",
                            format_duration(step_start.elapsed())
                        );
                    }
                    Some(Err(e)) => {
                        log::info!(
                            "[processor] LLM query failed after {}",
                            format_duration(step_start.elapsed())
                        );
                        consecutive_errors += 1;
                        let delay_secs = (consecutive_errors as u64 * 5).min(60);
                        log::error!(
                            "Ollama query failed (consecutive error #{}): {:#}. Retrying in {}s...",
                            consecutive_errors,
                            e,
                            delay_secs
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(delay_secs)) => {}
                            _ = shutdown.cancelled() => {
                                log::info!("[processor] Shutdown signal received — exiting");
                                return Ok(());
                            }
                        }
                    }
                }
            }
            Err(e) => {
                log::error!("[processor] Failed to build prompt ({}): {:#}", format_duration(step_start.elapsed()), e);
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                    _ = shutdown.cancelled() => {
                        log::info!("[processor] Shutdown signal received — exiting");
                        return Ok(());
                    }
                }
            }
        }
    }
}

// ── Token counting ──

fn count_tokens(tokenizer: &Tokenizer, text: &str) -> usize {
    tokenizer
        .encode_fast(text, false)
        .map(|enc| enc.len())
        .unwrap_or_else(|e| {
            log::warn!(
                "tokenization failed ({}) — falling back to char-based estimate",
                e
            );
            // Conservative fallback: ~3.5 chars per token for English/mixed text
            text.len() / 3
        })
}

// ── Time window helpers ──

fn find_newest_message_time(snapshots: &[ChannelSnapshot]) -> Option<DateTime<Utc>> {
    snapshots
        .iter()
        .flat_map(|s| s.messages.iter())
        .map(|m| m.date)
        .max()
}

/// Collect messages from each channel that fall within the given time window,
/// providing UNIFORM history depth across all channels (same cutoff time for all).
fn messages_in_window<'a>(
    snapshots: &'a [ChannelSnapshot],
    newest: DateTime<Utc>,
    window: ChronoDuration,
) -> Vec<(&'a ChannelSnapshot, Vec<&'a ChannelMessage>)> {
    let cutoff = newest - window;
    snapshots
        .iter()
        .map(|snap| {
            let msgs: Vec<&ChannelMessage> =
                snap.messages.iter().filter(|m| m.date >= cutoff).collect();
            (snap, msgs)
        })
        .filter(|(_, msgs)| !msgs.is_empty())
        .collect()
}

fn total_message_count(snapshots: &[ChannelSnapshot]) -> usize {
    snapshots.iter().map(|s| s.messages.len()).sum()
}

fn windowed_message_count(
    channel_msgs: &[(&ChannelSnapshot, Vec<&ChannelMessage>)],
) -> usize {
    channel_msgs.iter().map(|(_, msgs)| msgs.len()).sum()
}

// ── Prompt construction ──

fn build_prompt_text(
    channel_messages: &[(&ChannelSnapshot, Vec<&ChannelMessage>)],
    newest: DateTime<Utc>,
    focus_minutes: i64,
) -> String {
    let focus_cutoff = newest - ChronoDuration::minutes(focus_minutes);
    let total_msgs: usize = channel_messages.iter().map(|(_, msgs)| msgs.len()).sum();
    let num_channels = channel_messages.len();

    // Pre-allocate generously: ~256 bytes per message + prompt boilerplate
    let mut prompt = String::with_capacity(total_msgs * 256 + 4096);

    writeln!(
        &mut prompt,
        "You are a real-time news analyst. Below are the latest messages from \
         {} Telegram news channels ({} messages total).",
        num_channels, total_msgs
    )
    .unwrap();
    writeln!(&mut prompt).unwrap();
    writeln!(
        &mut prompt,
        "YOUR TASK: Analyze ONLY the most recent {} minutes of activity \
         (from {} UTC to {} UTC). Earlier messages are provided for background context only.",
        focus_minutes,
        focus_cutoff.format("%H:%M:%S"),
        newest.format("%H:%M:%S"),
    )
    .unwrap();
    writeln!(&mut prompt).unwrap();
    writeln!(&mut prompt, "=== CHANNEL MESSAGES ===").unwrap();

    for (snap, msgs) in channel_messages {
        writeln!(&mut prompt).unwrap();
        writeln!(&mut prompt, "--- {} ---", snap.channel_title).unwrap();
        for msg in msgs {
            writeln!(
                &mut prompt,
                "[{} UTC] {}",
                msg.date.format("%Y-%m-%d %H:%M:%S"),
                msg.text
            )
            .unwrap();
        }
    }

    writeln!(&mut prompt).unwrap();
    writeln!(&mut prompt, "=== END MESSAGES ===").unwrap();
    writeln!(&mut prompt).unwrap();

    // The JSON template uses doubled braces {{ }} for literal braces in format strings.
    write!(
        &mut prompt,
        r#"Respond with ONLY a JSON object in the exact format below. No other text before or after the JSON.

{{
  "updates": [
    {{
      "channel": "channel name",
      "headline": "concise headline (max 15 words)",
      "importance": "critical|high|medium|low",
      "summary": "1-2 sentence information-dense summary"
    }}
  ],
  "meta": {{
    "channels_analyzed": {num_channels},
    "time_window_minutes": {focus_minutes},
    "generated_at": "{now}"
  }}
}}

Rules:
- Include ONLY genuinely notable news updates from the last {focus_minutes} minutes
- If nothing notable happened, return {{"updates": [], "meta": {{...}}}}
- Be information-dense and incredibly brief
- Categorize importance: critical (breaking/urgent), high (significant), medium (noteworthy), low (minor)
- Output ONLY valid JSON, nothing else"#,
        num_channels = num_channels,
        focus_minutes = focus_minutes,
        now = newest.to_rfc3339(),
    )
    .unwrap();

    prompt
}

// ── Token-budgeted prompt building (exponential expansion + binary search) ──

/// Build the largest possible prompt that fits under MAX_TOKENS.
///
/// Algorithm:
///   1. Start with a 15-minute window (the focus period).
///   2. Double the window repeatedly (exponential expansion) to include more context.
///   3. When the token count exceeds budget (or all messages are included), stop expanding.
///   4. Binary search between the last-good window and the over-budget window
///      to find the precise optimal fit.
///   5. The time window is uniform across all channels — same cutoff for everyone.
///
/// Returns: (prompt_string, window_in_minutes, token_count)
fn build_budgeted_prompt(
    snapshots: &[ChannelSnapshot],
    tokenizer: &Tokenizer,
) -> Result<(String, u64, usize)> {
    let newest = find_newest_message_time(snapshots)
        .context("no messages found in any channel buffer — nothing to analyze")?;

    let total_all = total_message_count(snapshots);

    // Phase 1: Exponential expansion to find the range boundaries.
    let mut window_minutes: u64 = FOCUS_MINUTES as u64;
    let mut last_good: Option<(String, u64, usize)> = None;

    loop {
        let window = ChronoDuration::minutes(window_minutes as i64);
        let channel_msgs = messages_in_window(snapshots, newest, window);

        if channel_msgs.is_empty() {
            // No messages in this window yet. If we've expanded far enough, give up.
            if window_minutes >= 60 * 24 {
                break;
            }
            window_minutes *= 2;
            continue;
        }

        let prompt = build_prompt_text(&channel_msgs, newest, FOCUS_MINUTES);
        let tokens = count_tokens(tokenizer, &prompt);

        if tokens >= MAX_TOKENS {
            // Over budget — binary search between last_good and current window.
            break;
        }

        // Under budget. Save this as the best known good result.
        last_good = Some((prompt, window_minutes, tokens));

        // Check if we've included every message across all channels.
        let included = windowed_message_count(&channel_msgs);
        if included >= total_all {
            return Ok(last_good.unwrap());
        }

        // Double the window for the next iteration.
        window_minutes = window_minutes.saturating_mul(2);

        // Safety: don't search beyond 7 days (absurdly generous).
        if window_minutes > 60 * 24 * 7 {
            break;
        }
    }

    // Phase 2: Binary search refinement.
    if let Some((good_prompt, good_minutes, good_tokens)) = last_good {
        let high = window_minutes; // This window was over budget (or hit the ceiling).
        match binary_search_window(snapshots, tokenizer, newest, good_minutes, high) {
            Some(refined) => Ok(refined),
            None => Ok((good_prompt, good_minutes, good_tokens)),
        }
    } else {
        // Even the smallest window (FOCUS_MINUTES) exceeded the budget.
        // Binary search between 1 minute and FOCUS_MINUTES to find what fits.
        match binary_search_window(snapshots, tokenizer, newest, 1, FOCUS_MINUTES as u64) {
            Some(result) => Ok(result),
            None => {
                // Even 1 minute exceeds budget. Build it anyway — it's the minimum we can do.
                log::warn!(
                    "even a 1-minute context window exceeds the {} token budget — \
                     channels are extremely active, prompt will be over budget",
                    MAX_TOKENS
                );
                let window = ChronoDuration::minutes(1);
                let channel_msgs = messages_in_window(snapshots, newest, window);
                let prompt = build_prompt_text(&channel_msgs, newest, FOCUS_MINUTES);
                let tokens = count_tokens(tokenizer, &prompt);
                Ok((prompt, 1, tokens))
            }
        }
    }
}

/// Binary search between `low_minutes` and `high_minutes` to find the largest
/// window where the prompt fits under MAX_TOKENS.
fn binary_search_window(
    snapshots: &[ChannelSnapshot],
    tokenizer: &Tokenizer,
    newest: DateTime<Utc>,
    low_minutes: u64,
    high_minutes: u64,
) -> Option<(String, u64, usize)> {
    let mut low = low_minutes;
    let mut high = high_minutes;
    let mut best: Option<(String, u64, usize)> = None;

    // Refine until precision is within 1 minute.
    while high.saturating_sub(low) > 1 {
        let mid = low + (high - low) / 2;
        let window = ChronoDuration::minutes(mid as i64);
        let channel_msgs = messages_in_window(snapshots, newest, window);

        if channel_msgs.is_empty() {
            low = mid;
            continue;
        }

        let prompt = build_prompt_text(&channel_msgs, newest, FOCUS_MINUTES);
        let tokens = count_tokens(tokenizer, &prompt);

        if tokens < MAX_TOKENS {
            best = Some((prompt, mid, tokens));
            low = mid;
        } else {
            high = mid;
        }
    }

    best
}

// ── Output processing ──

/// Parse the LLM response as JSON, print a human-readable summary to stdout.
/// If the JSON is malformed, print the raw output with a warning.
fn process_and_print_output(raw_response: &str) {
    let cleaned = strip_think_blocks(raw_response);
    let json_str = extract_json(&cleaned);

    match serde_json::from_str::<NewsOutput>(json_str) {
        Ok(output) => {
            if output.updates.is_empty() {
                println!(
                    "[{}] No notable updates in the last {} minutes",
                    Utc::now().format("%H:%M:%S UTC"),
                    FOCUS_MINUTES
                );
            } else {
                println!(
                    "\n=== NEWS UPDATE [{}] ({} items) ===",
                    Utc::now().format("%H:%M:%S UTC"),
                    output.updates.len()
                );
                for item in &output.updates {
                    println!(
                        "  [{}] {} — {}\n         {}",
                        item.importance.to_uppercase(),
                        item.channel,
                        item.headline,
                        item.summary
                    );
                }
                println!("=== END UPDATE ===\n");
            }
        }
        Err(e) => {
            log::warn!(
                "LLM response was not valid JSON ({}) — printing raw. \
                 First 500 chars of cleaned output: '{}'",
                e,
                &cleaned[..500.min(cleaned.len())]
            );
            println!(
                "[{}] (raw) {}",
                Utc::now().format("%H:%M:%S UTC"),
                cleaned.trim()
            );
        }
    }
}

/// Strip `<think>...</think>` blocks that Qwen 3.5 may produce before the JSON.
fn strip_think_blocks(text: &str) -> String {
    let mut result = text.to_string();
    // Repeatedly strip think blocks (there should be at most one, but be safe).
    while let Some(start) = result.find("<think>") {
        if let Some(end_tag_start) = result[start..].find("</think>") {
            let end = start + end_tag_start + "</think>".len();
            result = format!("{}{}", &result[..start], &result[end..]);
        } else {
            // Unclosed <think> — strip from <think> to end of string.
            result.truncate(start);
            break;
        }
    }
    result
}

/// Extract the first JSON object from text by finding the outermost `{` and `}`.
/// Handles cases where the LLM includes preamble or postamble around the JSON.
fn extract_json(text: &str) -> &str {
    let trimmed = text.trim();
    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            if end > start {
                return &trimmed[start..=end];
            }
        }
    }
    trimmed
}
