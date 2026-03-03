use crate::buffer::{ChannelMessage, ChannelBuffers, ChannelSnapshot};
use crate::ollama::OllamaClient;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Deserialize;
use std::fmt::Write as _;
use std::time::Duration;
use tokenizers::Tokenizer;

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

/// The main LLM processing loop. Runs forever (until the task is cancelled):
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
) -> Result<()> {
    let mut consecutive_errors: u32 = 0;

    loop {
        let snapshot = buffers.snapshot();

        // Wait if no messages have arrived yet.
        if snapshot.is_empty() {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        let total_msgs: usize = snapshot.iter().map(|s| s.messages.len()).sum();
        if total_msgs < MIN_MESSAGES_TO_PROCESS {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        // Build a token-budgeted prompt via binary search on time window.
        match build_budgeted_prompt(&snapshot, &tokenizer) {
            Ok((prompt, window_minutes, token_count)) => {
                log::info!(
                    "Querying LLM: {} channels, {} messages, {}min context window, ~{} tokens",
                    snapshot.len(),
                    total_msgs,
                    window_minutes,
                    token_count,
                );

                match ollama.query(&prompt).await {
                    Ok(response) => {
                        consecutive_errors = 0;
                        process_and_print_output(&response);
                    }
                    Err(e) => {
                        consecutive_errors += 1;
                        let delay_secs = (consecutive_errors as u64 * 5).min(60);
                        log::error!(
                            "Ollama query failed (consecutive error #{}): {:#}. Retrying in {}s...",
                            consecutive_errors,
                            e,
                            delay_secs
                        );
                        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                    }
                }
            }
            Err(e) => {
                log::error!("Failed to build LLM prompt: {:#}", e);
                tokio::time::sleep(Duration::from_secs(10)).await;
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
