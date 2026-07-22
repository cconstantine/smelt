//! Rust types for the subset of the Claude Messages API used by smelt.
//! Serialized/deserialized directly against `https://api.anthropic.com/v1/messages`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: Vec<ContentBlock>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CreateMessageRequest {
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<AnthropicMessage>,
    pub stream: bool,
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
            serde_json::to_value(ContentBlock::Text { text: "hi".to_string() }).unwrap(),
            serde_json::json!({"type": "text", "text": "hi"})
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
            vec![ContentBlock::Text { text: "HELLO".to_string() }]
        );
    }
}
