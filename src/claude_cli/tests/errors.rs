//! A CLI run's failures, read from the S5 capture's real `result` events
//! and, for the subscription limit, a synthesized one (no capture of a
//! real limit exists: it is marked so).

use crate::claude_cli::errors::*;

/// The fixture's `result` event.
fn result_of(fixture: &str) -> serde_json::Value {
    fixture
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("a fixture line"))
        .find(|event| event["type"] == "result")
        .expect("a result")
}

#[test]
fn test_a_refused_token_says_how_to_sign_in() {
    let error = result_error(&result_of(include_str!("../fixtures/bad_token.jsonl")), None).expect("an error");
    assert!(error.starts_with(NOT_SIGNED_IN), "{error}");
    assert!(error.contains("OAuth access token is invalid"), "the CLI's words are quoted: {error}");
}

#[test]
fn test_an_unknown_model_quotes_the_cli() {
    let error = result_error(&result_of(include_str!("../fixtures/bad_model.jsonl")), None).expect("an error");
    assert!(error.starts_with("Claude Code failed: There's an issue with the selected model (claude-nonexistent-0)"), "{error}");
}

#[test]
fn test_a_reached_limit_says_which_and_when_it_resets() {
    // Synthesized: the S6 capture's rate_limit_event, rejected, and a 429
    // result as the API gives one. Not captured from a real limit.
    let rate = RateLimit::from_event(&serde_json::json!({
        "type": "rate_limit_event",
        "rate_limit_info": {"status": "rejected", "rateLimitType": "five_hour", "resetsAt": 1_767_225_600},
    }));
    let result = serde_json::json!({"type": "result", "is_error": true, "api_error_status": 429, "result": "API Error: 429 rate limited"});
    let error = result_error(&result, Some(&rate)).expect("an error");
    assert_eq!(error, "Your Claude subscription's usage limit (five-hour) is reached. It resets at 2026-01-01 00:00 UTC. (Claude Code said: API Error: 429 rate limited)");
    let allowed = RateLimit { status: "allowed".into(), ..rate };
    let other = serde_json::json!({"type": "result", "is_error": true, "result": "boom"});
    assert_eq!(result_error(&other, Some(&allowed)).as_deref(), Some("Claude Code failed: boom"), "an allowed limit isn't the cause");
}

#[test]
fn test_a_successful_result_is_no_error_and_long_words_are_cut() {
    let ok = result_of(include_str!("../fixtures/loop.jsonl"));
    assert_eq!(result_error(&ok, None), None);
    let long = serde_json::json!({"is_error": true, "result": "y".repeat(5_000)});
    let error = result_error(&long, None).expect("an error");
    assert!(error.chars().count() < MAX_QUOTED + 40, "{}", error.len());
}
