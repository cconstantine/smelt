//! A CLI run's failures as the turn says them (SME-430): its `result`
//! event with `is_error`, read with the last `rate_limit_event` before it.
//! The shapes are the S5 capture's (`fixtures/bad_token.jsonl`,
//! `bad_model.jsonl`).

use serde_json::Value;

/// The most of the CLI's own words a turn's error quotes.
pub const MAX_QUOTED: usize = 2_000;

/// What the turn says when the CLI isn't signed in, or its token was
/// refused.
pub const NOT_SIGNED_IN: &str = "Claude Code isn't signed in, or Anthropic refused its sign-in. Run claude setup-token on your own machine, set what it prints as CLAUDE_CODE_OAUTH_TOKEN in the smelt server's environment, and restart the server.";

/// The subscription's limit, as the CLI's `rate_limit_event` last said it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RateLimit {
    /// `allowed`, `allowed_warning` or `rejected`.
    pub status: String,
    /// Which limit: `five_hour`, `seven_day`, …
    pub kind: Option<String>,
    /// When it resets, in Unix seconds.
    pub resets_at: Option<i64>,
}

impl RateLimit {
    /// A `rate_limit_event`'s `rate_limit_info`.
    pub fn from_event(event: &Value) -> Self {
        let info = &event["rate_limit_info"];
        Self {
            status: info["status"].as_str().unwrap_or_default().to_string(),
            kind: info["rateLimitType"].as_str().map(str::to_string),
            resets_at: info["resetsAt"].as_i64(),
        }
    }
}

/// `text` cut to `MAX_QUOTED` characters.
fn quoted(text: &str) -> String {
    quoted_to(text, MAX_QUOTED)
}

/// The most of the CLI's stderr a turn's error quotes.
pub const MAX_STDERR_LINE: usize = 200;

/// The last line of the CLI's stderr, for a turn's error: at most
/// `MAX_STDERR_LINE` characters, with every path, email and token-shaped
/// word taken out (review 1 L4). The conversation keeps this text.
pub fn scrubbed_line(stderr: &str) -> String {
    static SCRUB: std::sync::LazyLock<Vec<(regex::Regex, &'static str)>> = std::sync::LazyLock::new(|| {
        [
            (r"[\w.+-]+@[\w-]+(\.[\w-]+)+", "<email>"),
            (r"sk-ant-[\w-]+", "<token>"),
            (r"\b[A-Za-z0-9_-]{32,}\b", "<token>"),
            (r"(~|\.{1,2})?/[^\s'\x22,;:)]+", "<path>"),
        ]
        .into_iter()
        .filter_map(|(pattern, with)| regex::Regex::new(pattern).ok().map(|re| (re, with)))
        .collect()
    });
    let Some(line) = stderr.lines().map(str::trim).filter(|line| !line.is_empty()).last() else {
        return String::new();
    };
    let mut line = line.to_string();
    for (pattern, with) in SCRUB.iter() {
        line = pattern.replace_all(&line, *with).into_owned();
    }
    quoted_to(&line, MAX_STDERR_LINE)
}

fn quoted_to(text: &str, most: usize) -> String {
    let mut cut: String = text.chars().take(most).collect();
    if cut.len() < text.len() {
        cut.push('\u{2026}');
    }
    cut
}

/// The turn's error for a `result` event, or `None` when it isn't one.
pub fn result_error(result: &Value, rate: Option<&RateLimit>) -> Option<String> {
    if result["is_error"].as_bool() != Some(true) {
        return None;
    }
    let said = result["result"]
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            let errors: Vec<&str> = result["errors"].as_array()?.iter().filter_map(Value::as_str).collect();
            (!errors.is_empty()).then(|| errors.join("; "))
        })
        .unwrap_or_else(|| format!("it ended with {}", result["subtype"].as_str().unwrap_or("an error")));
    let status = result["api_error_status"].as_i64();
    if status == Some(401) || status == Some(403) {
        return Some(format!("{NOT_SIGNED_IN} (Claude Code said: {})", quoted(&said)));
    }
    let rejected = rate.filter(|rate| rate.status == "rejected");
    if status == Some(429) || rejected.is_some() {
        let which = rejected.and_then(|r| r.kind.as_deref()).map(|kind| format!(" ({})", kind.replace('_', "-"))).unwrap_or_default();
        let when = rejected
            .and_then(|r| r.resets_at)
            .and_then(|at| chrono::DateTime::from_timestamp(at, 0))
            .map(|at| format!(" It resets at {} UTC.", at.format("%Y-%m-%d %H:%M")))
            .unwrap_or_default();
        return Some(format!("Your Claude subscription's usage limit{which} is reached.{when} (Claude Code said: {})", quoted(&said)));
    }
    Some(format!("Claude Code failed: {}", quoted(&said)))
}
