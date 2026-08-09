use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::anthropic::ContentBlock;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct Conversation {
    pub id: i64,
    pub title: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct Message {
    pub id: i64,
    pub conversation_id: i64,
    pub role: String,
    pub content: String,
    pub created_at: NaiveDateTime,
}

impl Message {
    /// Parses the stored JSON `content` column into the Anthropic
    /// `ContentBlock` shape it's serialized as. Returns the underlying
    /// `serde_json::Error` on malformed content so the caller can surface it
    /// rather than silently rendering nothing.
    pub fn blocks(&self) -> Result<Vec<ContentBlock>, serde_json::Error> {
        serde_json::from_str(&self.content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message_with_content(content: &str) -> Message {
        Message {
            id: 1,
            conversation_id: 1,
            role: "user".to_string(),
            content: content.to_string(),
            created_at: chrono::Utc::now().naive_utc(),
        }
    }

    #[test]
    fn test_blocks_parses_stored_json_text_block() {
        let message = message_with_content(r#"[{"type":"text","text":"hello"}]"#);
        assert_eq!(
            message.blocks().expect("valid JSON should parse"),
            vec![ContentBlock::Text {
                text: "hello".to_string()
            }]
        );
    }

    #[test]
    fn test_blocks_errors_on_malformed_content() {
        let message = message_with_content("not json");
        assert!(message.blocks().is_err());
    }
}
