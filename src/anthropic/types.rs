//! Rust types for the subset of the Claude Messages API used by smelt.
//! Serialized/deserialized directly against `https://api.anthropic.com/v1/messages`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    /// The model's reasoning, when `CreateMessageRequest.thinking` is set —
    /// always the *first* block in an assistant turn's content when
    /// present. `signature` is an opaque token Anthropic uses to verify
    /// this block wasn't tampered with if it's echoed back in a later
    /// turn's history (as it is here — `run_turn` persists and replays
    /// `ContentBlock`s uninterpreted); never displayed, just carried
    /// through as-is.
    Thinking {
        thinking: String,
        signature: String,
    },
    /// A model-generated summary standing in for every original message up
    /// to and including `covers_through_message_id` — auto-compaction's own
    /// output, never something Anthropic itself sends or accepts. Persisted
    /// as an ordinary new message (nothing earlier is rewritten or
    /// deleted), but replayed to Anthropic as a plain `Text` block — see
    /// `api::chat::history_for_request` — since Anthropic has no concept of
    /// this block type. See docs/projects/plans/auto-compaction.md.
    CompactionSummary {
        summary: String,
        covers_through_message_id: i64,
    },
    /// A fixed, structural message auto-compaction inserts immediately
    /// before and after a `CompactionSummary` block (see
    /// `api::chat::compaction_messages`) — carries no real conversational
    /// content, just makes that synthetic exchange valid for Anthropic's
    /// own message-shape rules (`messages` must start with `user` and
    /// strictly alternate). Replayed to Anthropic as a plain `Text` block,
    /// same as `CompactionSummary`, but rendered as nothing in the
    /// transcript (see `render_block_element`) — a human reading it never
    /// typed or needs to see this, unlike the summary itself. See
    /// docs/projects/plans/auto-compaction.md.
    CompactionPlaceholder {
        text: String,
    },
}

/// Anthropic's own real `usage` field names, no translation layer — the
/// ground truth for "how much of the context window is actually being
/// used." Ungated (unlike the request/response types below): `stream.rs`
/// (server-only) constructs these from a real response, but
/// `events::ConversationEvent::ContextUsageUpdate` carries one across the
/// wire to the browser too, so the type itself must compile for both
/// `server` and `web` — see docs/projects/plans/auto-compaction.md.
/// `#[serde(default)]` on every field: `message_delta`'s own `usage`
/// object only ever carries `output_tokens`, not the other three, and
/// this same type deserializes both shapes. `sqlx::FromRow` gated the same
/// way `models::Conversation`/`Message` gate theirs — `db.rs` reads this
/// straight back out of `conversation_context_usage` by column name.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub cache_creation_input_tokens: i64,
    #[serde(default)]
    pub cache_read_input_tokens: i64,
}

// `AnthropicMessage`/`CreateMessageRequest` below are only built and sent
// by `stream.rs`, which is itself server-only — the `web` (browser) build
// never touches them.
#[cfg(feature = "server")]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: Vec<ContentBlock>,
}

/// Ungated, unlike `AnthropicMessage`/`CreateMessageRequest` above —
/// `api::chat::get_context_detail`'s context-visibility view sends every
/// available tool's full definition to the browser (see
/// docs/projects/plans/auto-compaction.md), so this specifically *is*
/// touched by the `web` build now, even though it started server-only in
/// the same PR that introduced this whole split.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// `{"type": "adaptive"}` — the model manages its own thinking budget
/// within `max_tokens` rather than a caller-specified `budget_tokens`
/// (deprecated on current models). The only variant smelt sends; kept as
/// an enum rather than a bare string so an unsupported value can't be
/// constructed by mistake.
#[cfg(feature = "server")]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingConfig {
    Adaptive,
}

#[cfg(feature = "server")]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CreateMessageRequest {
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<AnthropicMessage>,
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_usage_deserializes_a_full_message_start_usage_object() {
        let usage: TokenUsage = serde_json::from_value(serde_json::json!({
            "input_tokens": 2095,
            "cache_creation_input_tokens": 10,
            "cache_read_input_tokens": 5,
            "output_tokens": 3
        }))
        .expect("full usage object should deserialize");
        assert_eq!(
            usage,
            TokenUsage {
                input_tokens: 2095,
                output_tokens: 3,
                cache_creation_input_tokens: 10,
                cache_read_input_tokens: 5,
            }
        );
    }

    #[test]
    fn test_token_usage_deserializes_message_deltas_output_tokens_only_shape() {
        // message_delta's own `usage` object carries only output_tokens,
        // never the other three fields — must not fail to parse just
        // because they're absent.
        let usage: TokenUsage = serde_json::from_value(serde_json::json!({"output_tokens": 45}))
            .expect("output-tokens-only usage object should deserialize");
        assert_eq!(
            usage,
            TokenUsage {
                input_tokens: 0,
                output_tokens: 45,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            }
        );
    }

    #[test]
    fn test_content_block_wire_tag_matches_anthropic_api() {
        assert_eq!(
            serde_json::to_value(ContentBlock::Text {
                text: "hi".to_string()
            })
            .unwrap(),
            serde_json::json!({"type": "text", "text": "hi"})
        );
    }

    #[test]
    fn test_thinking_block_wire_tag_matches_anthropic_api() {
        assert_eq!(
            serde_json::to_value(ContentBlock::Thinking {
                thinking: "hmm".to_string(),
                signature: "sig123".to_string(),
            })
            .unwrap(),
            serde_json::json!({"type": "thinking", "thinking": "hmm", "signature": "sig123"})
        );
    }

    #[test]
    fn test_compaction_summary_block_wire_shape() {
        // Storage/frontend shape only — this block never round-trips
        // through Anthropic's own API itself, see its doc comment.
        assert_eq!(
            serde_json::to_value(ContentBlock::CompactionSummary {
                summary: "earlier discussion condensed".to_string(),
                covers_through_message_id: 42,
            })
            .unwrap(),
            serde_json::json!({
                "type": "compaction_summary",
                "summary": "earlier discussion condensed",
                "covers_through_message_id": 42
            })
        );
    }

    #[test]
    fn test_compaction_placeholder_block_wire_shape() {
        // Storage/frontend shape only — same "never round-trips through
        // Anthropic's own API" caveat as CompactionSummary.
        assert_eq!(
            serde_json::to_value(ContentBlock::CompactionPlaceholder {
                text: "Continue based on the summary above.".to_string(),
            })
            .unwrap(),
            serde_json::json!({
                "type": "compaction_placeholder",
                "text": "Continue based on the summary above."
            })
        );
    }

    #[test]
    fn test_thinking_config_wire_shape() {
        assert_eq!(
            serde_json::to_value(ThinkingConfig::Adaptive).unwrap(),
            serde_json::json!({"type": "adaptive"})
        );
    }

    #[test]
    fn test_request_omits_thinking_when_none() {
        let req = CreateMessageRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 4096,
            system: None,
            messages: vec![],
            stream: true,
            tools: vec![],
            thinking: None,
        };
        let value = serde_json::to_value(&req).unwrap();
        assert!(
            value.get("thinking").is_none(),
            "thinking key should be omitted entirely when None, got: {value:?}"
        );
    }

    #[test]
    fn test_request_includes_thinking_when_set() {
        let req = CreateMessageRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 4096,
            system: None,
            messages: vec![],
            stream: true,
            tools: vec![],
            thinking: Some(ThinkingConfig::Adaptive),
        };
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(
            value.get("thinking"),
            Some(&serde_json::json!({"type": "adaptive"}))
        );
    }

    #[test]
    fn test_tool_use_block_wire_tag_matches_anthropic_api() {
        assert_eq!(
            serde_json::to_value(ContentBlock::ToolUse {
                id: "toolu_01".to_string(),
                name: "add".to_string(),
                input: serde_json::json!({"a": 1, "b": 2}),
            })
            .unwrap(),
            serde_json::json!({
                "type": "tool_use",
                "id": "toolu_01",
                "name": "add",
                "input": {"a": 1, "b": 2}
            })
        );
    }

    #[test]
    fn test_tool_result_block_omits_is_error_when_none() {
        let value = serde_json::to_value(ContentBlock::ToolResult {
            tool_use_id: "toolu_01".to_string(),
            content: "3".to_string(),
            is_error: None,
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "type": "tool_result",
                "tool_use_id": "toolu_01",
                "content": "3"
            })
        );
    }

    #[test]
    fn test_tool_result_block_includes_is_error_when_some() {
        let value = serde_json::to_value(ContentBlock::ToolResult {
            tool_use_id: "toolu_01".to_string(),
            content: "boom".to_string(),
            is_error: Some(true),
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "type": "tool_result",
                "tool_use_id": "toolu_01",
                "content": "boom",
                "is_error": true
            })
        );
    }

    #[test]
    fn test_tools_field_omitted_when_empty() {
        let req = CreateMessageRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 4096,
            system: None,
            messages: vec![],
            stream: true,
            tools: vec![],
            thinking: None,
        };
        let value = serde_json::to_value(&req).unwrap();
        assert!(
            value.get("tools").is_none(),
            "tools key should be omitted entirely when empty, got: {value:?}"
        );
    }

    #[test]
    fn test_tool_definition_wire_shape() {
        let tool = ToolDefinition {
            name: "add".to_string(),
            description: "Add two numbers".to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        };
        assert_eq!(
            serde_json::to_value(&tool).unwrap(),
            serde_json::json!({
                "name": "add",
                "description": "Add two numbers",
                "input_schema": {"type": "object", "properties": {}}
            })
        );
    }

    #[test]
    fn test_request_omits_system_when_none() {
        let req = CreateMessageRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 4096,
            system: None,
            messages: vec![],
            stream: true,
            tools: vec![],
            thinking: None,
        };
        let value = serde_json::to_value(&req).unwrap();
        assert!(
            value.get("system").is_none(),
            "system key should be omitted entirely when None, got: {value:?}"
        );
    }
}
