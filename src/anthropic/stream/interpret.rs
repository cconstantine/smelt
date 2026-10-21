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
    /// A `message_delta`: its `stop_reason`, if it has one, and its
    /// `usage`, read with or without a stop reason (SME-178). One with
    /// neither is `Ignored`.
    Delta {
        stop_reason: Option<String>,
        usage: DeltaUsage,
    },
    /// `message_start`'s own `usage` — the input/cache token counts for
    /// this call, unless a `message_delta` replaces them (`DeltaUsage`).
    /// `output_tokens` here is a placeholder Anthropic sends before any
    /// output has streamed; a `Delta`'s `output_tokens` (arriving later)
    /// is what actually finalizes it — see `stream_anthropic_message`.
    InitialUsage(TokenUsage),
    Error(String),
    Ignored,
}

/// A `message_delta`'s `usage`, each count `None` when the event left it
/// out (`TokenUsage`'s `serde(default)` can't tell a missing count from
/// zero). Counts in it are cumulative for the call.
///
/// Where providers put their counts (SME-178, B26): Anthropic sends input
/// and cache counts in `message_start` and usually only `output_tokens`
/// here, but a server tool's work raises the delta's `input_tokens`;
/// Ollama's `message_start` has only its request-size estimate, and its
/// delta the real counts; LiteLLM's `/v1/messages` adapter sends zeros in
/// `message_start` and the real counts here, or placeholder zeros when
/// its upstream sent no usage. llama.cpp sends real counts in
/// `message_start` and only `output_tokens` here.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DeltaUsage {
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_creation_input_tokens: Option<i64>,
    pub cache_read_input_tokens: Option<i64>,
}

impl DeltaUsage {
    fn from_value(usage: &Value) -> Self {
        let count = |key: &str| usage.get(key).and_then(Value::as_i64);
        Self {
            input_tokens: count("input_tokens"),
            output_tokens: count("output_tokens"),
            cache_creation_input_tokens: count("cache_creation_input_tokens"),
            cache_read_input_tokens: count("cache_read_input_tokens"),
        }
    }

    /// Applies this delta to the call's usage so far: a present
    /// `output_tokens` replaces the old one. The input side is replaced
    /// only when one of its counts here is non-zero, each count present
    /// replacing and each missing one kept; all zero or missing is a
    /// placeholder (LiteLLM's) or a plain Anthropic delta, and
    /// `message_start`'s counts stand.
    pub fn apply_to(&self, usage: &mut TokenUsage) {
        if let Some(output) = self.output_tokens {
            usage.output_tokens = output;
        }
        let input_side = [self.input_tokens, self.cache_creation_input_tokens, self.cache_read_input_tokens];
        if input_side.iter().any(|count| count.is_some_and(|c| c != 0)) {
            if let Some(input) = self.input_tokens {
                usage.input_tokens = input;
            }
            if let Some(creation) = self.cache_creation_input_tokens {
                usage.cache_creation_input_tokens = creation;
            }
            if let Some(read) = self.cache_read_input_tokens {
                usage.cache_read_input_tokens = read;
            }
        }
    }
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
        Some("message_delta") => {
            let stop_reason = value
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let usage = value.get("usage").map(DeltaUsage::from_value).unwrap_or_default();
            if stop_reason.is_none() && usage == DeltaUsage::default() {
                StreamOutcome::Ignored
            } else {
                StreamOutcome::Delta { stop_reason, usage }
            }
        }
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
