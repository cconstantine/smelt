//! Reading one Anthropic SSE event: what the stream loop acts on.

use serde_json::Value;

use super::TokenUsage;

/// A single interpreted Anthropic SSE payload, reduced to what
/// `stream_anthropic_message` needs to act on. Anthropic's stream carries
/// several event types (message_start, content_block_start,
/// content_block_delta, content_block_stop, message_delta, message_stop) —
/// only text/thinking deltas and errors matter for v1, everything else is
/// ignored (including `redacted_thinking` blocks — a rare safety-filtered
/// variant with a different, delta-less shape, not modeled here).
#[derive(Debug, Clone, PartialEq)]
pub enum StreamOutcome {
    TextDelta(String),
    ThinkingDelta(String),
    ThinkingSignatureDelta(String),
    ToolUseStart {
        id: String,
        name: String,
    },
    ToolUseInputDelta(String),
    BlockStop,
    /// `output_tokens` is `None` only if a `message_delta` event genuinely
    /// carried no `usage` field at all (defensive — real Anthropic
    /// responses always include one alongside `stop_reason`) — distinct
    /// from a real, explicit zero.
    StopReason {
        reason: String,
        output_tokens: Option<i64>,
    },
    /// `message_start`'s own `usage` — the authoritative input/cache token
    /// counts for this call. `output_tokens` here is a placeholder Anthropic
    /// sends before any output has streamed; `StopReason`'s own
    /// `output_tokens` (from `message_delta`, arriving later) is what
    /// actually finalizes it — see `stream_anthropic_message`.
    InitialUsage(TokenUsage),
    Error(String),
    Ignored,
}

/// Parse one decoded JSON payload from an Anthropic SSE `data:` line.
/// Pure and synchronous — unit-testable without any network access.
pub(super) fn interpret_stream_event(value: &Value) -> StreamOutcome {
    match value.get("type").and_then(Value::as_str) {
        Some("message_start") => {
            let usage = value.get("message").and_then(|m| m.get("usage"));
            match usage.and_then(|u| serde_json::from_value::<TokenUsage>(u.clone()).ok()) {
                Some(usage) => StreamOutcome::InitialUsage(usage),
                None => StreamOutcome::Ignored,
            }
        }
        Some("content_block_start") => {
            let block = value.get("content_block");
            let is_tool_use =
                block.and_then(|b| b.get("type")).and_then(Value::as_str) == Some("tool_use");
            if is_tool_use {
                let id = block
                    .and_then(|b| b.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let name = block
                    .and_then(|b| b.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                StreamOutcome::ToolUseStart {
                    id: id.to_string(),
                    name: name.to_string(),
                }
            } else {
                StreamOutcome::Ignored
            }
        }
        Some("content_block_delta") => {
            match value
                .get("delta")
                .and_then(|d| d.get("type"))
                .and_then(Value::as_str)
            {
                Some("text_delta") => {
                    let text = value
                        .get("delta")
                        .and_then(|d| d.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    StreamOutcome::TextDelta(text.to_string())
                }
                Some("input_json_delta") => {
                    let partial_json = value
                        .get("delta")
                        .and_then(|d| d.get("partial_json"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    StreamOutcome::ToolUseInputDelta(partial_json.to_string())
                }
                Some("thinking_delta") => {
                    let thinking = value
                        .get("delta")
                        .and_then(|d| d.get("thinking"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    StreamOutcome::ThinkingDelta(thinking.to_string())
                }
                Some("signature_delta") => {
                    let signature = value
                        .get("delta")
                        .and_then(|d| d.get("signature"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    StreamOutcome::ThinkingSignatureDelta(signature.to_string())
                }
                _ => StreamOutcome::Ignored,
            }
        }
        Some("content_block_stop") => StreamOutcome::BlockStop,
        Some("message_delta") => match value
            .get("delta")
            .and_then(|d| d.get("stop_reason"))
            .and_then(Value::as_str)
        {
            Some(reason) => StreamOutcome::StopReason {
                reason: reason.to_string(),
                output_tokens: value
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_i64),
            },
            None => StreamOutcome::Ignored,
        },
        Some("error") => {
            let message = value
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unknown error from Anthropic");
            StreamOutcome::Error(message.to_string())
        }
        _ => StreamOutcome::Ignored,
    }
}

/// The `index` a `content_block_*` event names, the block it belongs to.
pub(super) fn block_index(value: &Value) -> Option<usize> {
    value
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|i| usize::try_from(i).ok())
}
