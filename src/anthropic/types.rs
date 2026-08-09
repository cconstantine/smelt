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
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: Vec<ContentBlock>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

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
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CreateMessageResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        let value = serde_json::to_value(&req).unwrap();
        assert!(
            value.get("system").is_none(),
            "system key should be omitted entirely when None, got: {value:?}"
        );
    }

    #[test]
    fn test_response_ignores_unmodeled_fields() {
        // Real API responses carry id/model/role/usage/etc. we don't model —
        // deserialization must not choke on them.
        let raw = serde_json::json!({
            "id": "msg_1",
            "model": "claude-opus-4-8",
            "role": "assistant",
            "usage": {"input_tokens": 10, "output_tokens": 5},
            "content": [{"type": "text", "text": "HELLO"}],
            "stop_reason": "end_turn"
        });
        let response: CreateMessageResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(response.stop_reason, "end_turn");
        assert_eq!(
            response.content,
            vec![ContentBlock::Text {
                text: "HELLO".to_string()
            }]
        );
    }
}
