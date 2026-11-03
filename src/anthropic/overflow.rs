//! Telling a provider refusal that means "this request didn't fit the
//! model's context window" apart from every other model error (SME-73).
//! What a turn does with one is `turn`'s business; this module only
//! reads a refusal.
//!
//! Every pattern here is cited from a real reply, source-read at:
//! Anthropic's errors docs; llama.cpp `server-context.cpp` (de7fa0a);
//! Ollama's `llama_server.go` (c2b7368, with context shift off); vLLM
//! `renderers/params.py` (ab905a8); LiteLLM (577d74c); OpenRouter's
//! metadata type. hermes-agent PR #126600 saved 188240 — the *input*
//! count of an "A + M > W" refusal — as the model's window: the mistake
//! `numbers` exists not to make, hence windows only from known shapes.

use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

use super::stream::provider_error_message;

/// The smallest context window a refusal could plausibly mean. A
/// smaller number is a misread of one — and misreading one is the whole
/// danger of parsing numbers (SME-73).
const MIN_PLAUSIBLE_WINDOW: u32 = 1_024;

/// A refusal that means "this didn't fit the model's context window".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overflow {
    /// The window the refusal names, when it names one — read from a
    /// known shape only, and only a plausible one.
    pub window: Option<u32>,
    /// The input size the refusal names, when it names one.
    pub input_tokens: Option<u64>,
    /// The refusal as the provider phrased it.
    pub message: String,
}

/// Wordings matched case-insensitively on the refusal's message, each
/// from a cited body. A message may hold numbers these don't read: the
/// words answer only whether this is an overflow at all.
const WORDINGS: [&str; 8] = [
    "prompt is too long",                 // Anthropic, every model
    "exceed context limit",               // Anthropic, pre-Claude-4.5
    "exceeds the available context size", // llama.cpp
    "larger than the max context size",   // llama.cpp, the other wording
    "maximum context length",             // vLLM, and LiteLLM relaying it
    "longer than the context length",     // Ollama, with context shift off
    "contextwindowexceedederror",         // LiteLLM's prefix
    "context_length_exceeded",            // OpenRouter's metadata type
];

/// Whether refusing this request means it didn't fit. `status` is the
/// HTTP status, or `None` for a streamed `error` event: only those —
/// the transient statuses (429, 503, 529) say "later", never "too big",
/// and they are already retried before this is ever asked.
pub fn classify(status: Option<u16>, body: &str) -> Option<Overflow> {
    if !matches!(status, None | Some(400) | Some(413)) {
        return None;
    }
    // Typed before wording: a provider that names its error type says
    // what it is whatever its message has since been dressed in.
    let json = serde_json::from_str::<Value>(body).ok();
    let typed = json.as_ref().is_some_and(|j| {
        matches!(
            j.pointer("/error/type").and_then(Value::as_str),
            Some("exceed_context_size_error") | Some("request_too_large")
        ) || j.pointer("/error/metadata/error_type").and_then(Value::as_str)
            == Some("context_length_exceeded")
    });
    let message = provider_error_message(body);
    let lower = message.to_lowercase();
    if !typed && !WORDINGS.iter().any(|word| lower.contains(word)) {
        return None;
    }
    let (window, input_tokens) = numbers(&lower, json.as_ref());
    Some(Overflow {
        window,
        input_tokens,
        message,
    })
}

// The number shapes, each from a cited body (module docs), read on the
// lower-cased message. The patterns are constants, and the tests below
// exercise every one: an `expect` here can't fail.
#[expect(clippy::expect_used, reason = "constants; every pattern is exercised by the tests below")]
static TOO_LONG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d+) tokens > (\d+) maximum").expect("a valid regex"));
#[expect(clippy::expect_used, reason = "constants; every pattern is exercised by the tests below")]
static LLAMA_REQUEST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"request \((\d+) tokens\) exceeds the available context size \((\d+) tokens\)")
        .expect("a valid regex")
});
#[expect(clippy::expect_used, reason = "constants; every pattern is exercised by the tests below")]
static LLAMA_INPUT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"input \((\d+) tokens\) is larger than the max context size \((\d+) tokens\)")
        .expect("a valid regex")
});
#[expect(clippy::expect_used, reason = "constants; every pattern is exercised by the tests below")]
static MAX_CONTEXT_LENGTH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"maximum context length (?:is|of) (\d+)").expect("a valid regex"));
#[expect(clippy::expect_used, reason = "constants; every pattern is exercised by the tests below")]
static PLUS_OVER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d+) \+ (\d+) > (\d+)").expect("a valid regex"));
#[expect(clippy::expect_used, reason = "constants; every pattern is exercised by the tests below")]
static CONTAINS_INPUT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"contains (\d+) input tokens").expect("a valid regex"));
#[expect(clippy::expect_used, reason = "constants; every pattern is exercised by the tests below")]
static INPUT_MESSAGES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d+) tokens from the input messages").expect("a valid regex")
});

fn parsed<T: std::str::FromStr>(caps: &regex::Captures, group: usize) -> Option<T> {
    caps.get(group)?.as_str().parse().ok()
}

/// The window and the input size, from the known shapes and nowhere
/// else — never a bare number from a message that only counts tokens.
fn numbers(message: &str, json: Option<&Value>) -> (Option<u32>, Option<u64>) {
    let field = |key: &str| {
        json.and_then(|j| j.pointer(&format!("/error/{key}")).and_then(Value::as_u64))
    };
    // llama.cpp answers with the pair structured: n_ctx is the window.
    if let Some(window) = field("n_ctx").filter(|n| *n >= u64::from(MIN_PLAUSIBLE_WINDOW)) {
        return (u32::try_from(window).ok(), field("n_prompt_tokens"));
    }
    let window = TOO_LONG
        .captures(message)
        .or_else(|| LLAMA_REQUEST.captures(message))
        .or_else(|| LLAMA_INPUT.captures(message))
        .and_then(|caps| parsed::<u32>(&caps, 2))
        .or_else(|| MAX_CONTEXT_LENGTH.captures(message).and_then(|caps| parsed::<u32>(&caps, 1)))
        // "input length and max_tokens exceed context limit: A + M > W"
        // names the window as W, the *last* number; the first is the
        // input. (hermes-agent PR #126600 saved A as the window — the
        // mistake, caught live, that this shape exists to avoid.)
        .or_else(|| PLUS_OVER.captures(message).and_then(|caps| parsed::<u32>(&caps, 3)))
        .filter(|window| *window >= MIN_PLAUSIBLE_WINDOW);
    let input_tokens = TOO_LONG
        .captures(message)
        .or_else(|| LLAMA_REQUEST.captures(message))
        .or_else(|| LLAMA_INPUT.captures(message))
        .and_then(|caps| parsed::<u64>(&caps, 1))
        .or_else(|| CONTAINS_INPUT.captures(message).and_then(|caps| parsed::<u64>(&caps, 1)))
        .or_else(|| INPUT_MESSAGES.captures(message).and_then(|caps| parsed::<u64>(&caps, 1)))
        .or_else(|| PLUS_OVER.captures(message).and_then(|caps| parsed::<u64>(&caps, 1)));
    (window, input_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(body: &str) -> Option<Overflow> {
        classify(Some(400), body)
    }

    // --- bodies that mean "didn't fit" -----------------------------------

    /// Anthropic's own, from the errors docs — "prompt is too long:
    /// A tokens > W maximum". The window is the second number.
    #[test]
    fn test_anthropics_prompt_is_too_long_reads_the_window_past_the_input() {
        let o = refused(r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210242 tokens > 200000 maximum"},"request_id":"req_011Cxxxxxxxxxxxxxxxxxxx"}"#).expect("an overflow");
        assert_eq!((o.window, o.input_tokens), (Some(200_000), Some(210_242)));
        assert!(o.message.starts_with("prompt is too long"), "{}", o.message);
    }

    /// The hermes-agent mistake, as a test so it stays made impossible:
    /// from "input length and max_tokens exceed context limit: A + M > W"
    /// (pre-Claude-4.5 models) the window is W, the *last* number; PR
    /// #126600 saved A, 188240, as the window.
    #[test]
    fn test_the_pre_claude_4_5_refusal_names_the_window_not_its_first_number() {
        let o = refused(r#"{"type":"error","error":{"type":"invalid_request_error","message":"input length and max_tokens exceed context limit: 188240 + 128000 > 200000, decrease input length or max_tokens and try again"}}"#).expect("an overflow");
        assert_eq!((o.window, o.input_tokens), (Some(200_000), Some(188_240)));
    }

    /// llama.cpp answers structured: `n_ctx` is the window and
    /// `n_prompt_tokens` the input, read beside any wording.
    #[test]
    fn test_llama_cpp_reports_the_window_as_its_n_ctx() {
        let o = refused(r#"{"error":{"code":400,"type":"exceed_context_size_error","message":"request (33000 tokens) exceeds the available context size (32768 tokens), try increasing it","from_cache":false,"n_prompt_tokens":33000,"n_ctx":32768}}"#).expect("an overflow");
        assert_eq!((o.window, o.input_tokens), (Some(32_768), Some(33_000)));
    }

    /// ...and the same pair in its other wording, typed as a plain
    /// invalid request: the message alone names both numbers.
    #[test]
    fn test_llama_cpps_other_wording_names_both_numbers() {
        let o = refused(r#"{"error":{"code":400,"type":"invalid_request_error","message":"input (40000 tokens) is larger than the max context size (32768 tokens). skipping"}}"#).expect("an overflow");
        assert_eq!((o.window, o.input_tokens), (Some(32_768), Some(40_000)));
    }

    /// A 413 `request_too_large` is more bytes than the server takes —
    /// a request that didn't fit, with no window to learn whatever its
    /// message counts.
    #[test]
    fn test_anthropics_413_is_typed_and_counts_nothing() {
        let o = classify(Some(413), r#"{"type":"error","error":{"type":"request_too_large","message":"Request too large for model with 8000 maximum input token count"}}"#).expect("an overflow");
        assert_eq!((o.window, o.input_tokens), (None, None));
    }

    /// OpenRouter says it in its metadata; the message may say nothing.
    #[test]
    fn test_openrouter_is_named_by_its_metadata_error_type() {
        let o = refused(r#"{"error":{"message":"all providers refused this request","metadata":{"error_type":"context_length_exceeded"}}}"#).expect("an overflow");
        assert_eq!((o.window, o.input_tokens), (None, None));
    }

    /// Ollama refuses only with context shift off — and names no window
    /// (its loaded one is learned after the fact, SME-73).
    #[test]
    fn test_ollamas_refusal_says_nothing_about_the_window() {
        let o = refused(r#"{"error":{"type":"invalid_request_error","message":"the prompt is longer than the context length currently available to the model; shorten the prompt, or make num_ctx larger"}}"#).expect("an overflow");
        assert_eq!((o.window, o.input_tokens), (None, None));
    }

    /// vLLM's message, Anthropic-shaped by its router: both numbers,
    /// the window named as the model's maximum.
    #[test]
    fn test_vllm_names_the_window_and_the_input() {
        let o = refused(r#"{"type":"error","error":{"type":"invalid_request_error","message":"This model's maximum context length is 8192 tokens. However, you requested 4096 output tokens and your prompt contains 9000 input tokens, for a total of 13096 tokens. Please reduce the length of the input messages or the number of output tokens."}}"#).expect("an overflow");
        assert_eq!((o.window, o.input_tokens), (Some(8_192), Some(9_000)));
    }

    /// The older vLLM wording, as claude-code-router #1799 relayed it.
    #[test]
    fn test_the_older_vllm_wording_is_covered_too() {
        let o = refused(r#"{"error":{"type":"invalid_request_error","message":"This model's maximum context length of 4096 tokens. However, you requested 6000 tokens from the input messages. Please reduce the length of the input messages."}}"#).expect("an overflow");
        assert_eq!((o.window, o.input_tokens), (Some(4_096), Some(6_000)));
    }

    /// LiteLLM prefixes the upstream message with its typed error name.
    #[test]
    fn test_litellms_prefixed_message_is_an_overflow() {
        let o = refused(r#"{"error":{"message":"litellm.ContextWindowExceededError: litellm.BadRequestError: This model's maximum context length is 8192 tokens. However, your prompt contains 12000 input tokens."}}"#).expect("an overflow");
        assert_eq!((o.window, o.input_tokens), (Some(8_192), Some(12_000)));
    }

    /// An `error` event arriving mid-stream carries no status at all.
    #[test]
    fn test_an_sse_error_event_needs_no_status() {
        let o = classify(None, r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210242 tokens > 200000 maximum"}}"#).expect("an overflow");
        assert_eq!((o.window, o.input_tokens), (Some(200_000), Some(210_242)));
    }

    /// A server that answers with a plain-text body is read by its words.
    #[test]
    fn test_a_plain_text_refusal_is_read_by_its_words() {
        let o = refused("prompt is too long: 100000 tokens > 65536 maximum").expect("an overflow");
        assert_eq!((o.window, o.input_tokens), (Some(65_536), Some(100_000)));
    }

    /// A window under 1,024 is a misread, and the hermes mistake is a
    /// learned window too small: an overflow all the same, but the
    /// number is dropped and never learned.
    #[test]
    fn test_an_implausible_window_is_dropped() {
        let o = refused(r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 9000 tokens > 512 maximum"}}"#).expect("an overflow");
        assert_eq!((o.window, o.input_tokens), (None, Some(9_000)));
    }

    // --- and the set that must never count -------------------------------

    /// A 429's "too many tokens" is rate limiting, not size.
    #[test]
    fn test_a_rate_limit_is_never_an_overflow() {
        assert!(
            classify(Some(429), r#"{"type":"error","error":{"type":"rate_limit_error","message":"Too many tokens per day, please wait before trying again."}}"#).is_none(),
            "a 429 says later, not too big"
        );
    }

    /// The transient 5xx, and the probe log's Jinja 500, are statuses
    /// this is never asked about.
    #[test]
    fn test_the_transient_statuses_are_never_an_overflow() {
        assert!(classify(Some(529), r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#).is_none());
        assert!(classify(Some(500), r#"{"error":{"code":500,"message":"jinja2.exceptions.TemplateError: no template","type":"server_error"}}"#).is_none());
    }

    /// A thinking-binding refusal gets its own retry (`next_binding`)
    /// first; its wordings match none of these.
    #[test]
    fn test_a_thinking_binding_refusal_is_not_an_overflow() {
        assert!(refused(r#"{"type":"error","error":{"type":"invalid_request_error","message":"Invalid `signature`: this prompt is bound to a different conversation and cannot be replayed here"}}"#).is_none());
    }

    /// A server without prompt caching gets the settings hint, not a
    /// compaction.
    #[test]
    fn test_a_cache_control_refusal_is_not_an_overflow() {
        assert!(refused(r#"{"type":"error","error":{"type":"invalid_request_error","message":"Unexpected value(s) for the `cache_control` field: this server does not support prompt caching"}}"#).is_none());
    }

    /// A gateway hiding the real error can't be helped: nothing can
    /// classify what never arrives (SME-73 Trade-offs).
    #[test]
    fn test_a_gateway_hiding_the_real_error_stays_unrecognised() {
        assert!(refused(r#"{"error":{"message":"All target providers failed."}}"#).is_none());
    }

    /// Other 400s — an output-cap refusal, with its numbers — read
    /// unchanged.
    #[test]
    fn test_other_refusals_are_not_overflows() {
        assert!(refused(r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: 200000 > 128000, which is the maximum allowed number of output tokens for claude-opus-5"}}"#).is_none());
    }
}
