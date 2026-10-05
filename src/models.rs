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

/// What a conversation's completed model calls used and cost in all
/// (SME-106, `db::get_conversation_spend`). A stopped or failed call has no
/// final usage and isn't counted.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct ConversationSpend {
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub cache_read_input_tokens: i64,
    /// The sum over the calls that had a price; `None` when none did.
    pub cost_usd: Option<f64>,
    /// Calls with no price, left out of `cost_usd`.
    pub unpriced_calls: i64,
}

/// What `calls` cost, said for a person (SME-106): `cost_usd` over the
/// calls that had a price, `unpriced_calls` of them without one.
pub fn cost_text(calls: i64, cost_usd: Option<f64>, unpriced_calls: i64) -> String {
    if calls == 0 {
        return "No completed model calls yet.".to_string();
    }
    let Some(cost) = cost_usd else {
        return "No price for this model, so tokens only.".to_string();
    };
    // A cheap model's call costs a fraction of a cent: two places would
    // show it as nothing.
    let dollars = if cost > 0.0 && cost < 0.01 { format!("${cost:.4}") } else { format!("${cost:.2}") };
    match unpriced_calls {
        0 => dollars,
        1 => format!("{dollars}, plus 1 call with no price"),
        n => format!("{dollars}, plus {n} calls with no price"),
    }
}

/// What one provider's model was used for and cost over a period, for the
/// providers page (SME-106, `db::model_spend_since`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct ModelSpend {
    /// `None` once the provider is deleted.
    pub provider_name: Option<String>,
    pub model: String,
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cost_usd: Option<f64>,
    pub unpriced_calls: i64,
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

/// A language server the user has configured (SME-35): how to install and
/// run it, which files it handles, and what to send it. Data only: nothing
/// about any particular language is in smelt's code.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LanguageServer {
    pub id: i64,
    #[serde(flatten)]
    pub config: LanguageServerConfig,
    pub updated_at: NaiveDateTime,
}

/// A language server's settings, as the page edits them.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LanguageServerConfig {
    /// Lowercase letters, digits and dashes: it's part of the server pod's
    /// name, and what the model starts it by.
    pub name: String,
    /// The container image its pod runs, e.g. `rust:1`.
    pub image: String,
    /// A shell command run once when the pod starts, before the server can
    /// be used, e.g. `rustup component add rust-analyzer`. Empty for none.
    pub install_command: String,
    /// The server's executable, e.g. `rust-analyzer`.
    pub command: String,
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    /// File extension (without the dot) to LSP language id, e.g.
    /// `rs` → `rust`.
    pub file_types: std::collections::BTreeMap<String, String>,
    /// Files that mark a project's root, e.g. `Cargo.toml`.
    pub root_markers: Vec<String>,
    pub initialization_options: Option<serde_json::Value>,
    pub settings: Option<serde_json::Value>,
    pub memory_limit: String,
    pub cpu_limit: String,
    pub enabled: bool,
}

impl LanguageServerConfig {
    /// Why this can't be saved, or `Ok`.
    pub fn validate(&self) -> Result<(), String> {
        let name_ok = !self.name.is_empty()
            && self.name.len() <= 30
            && self.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !self.name.starts_with('-')
            && !self.name.ends_with('-');
        if !name_ok {
            return Err(
                "The name must be 1 to 30 lowercase letters, digits or dashes, not starting or ending with a dash."
                    .to_string(),
            );
        }
        if self.image.trim().is_empty() || self.image.chars().any(char::is_whitespace) {
            return Err("The image must be one image reference, e.g. rust:1.".to_string());
        }
        if self.command.trim().is_empty() {
            return Err("The command to run the server is missing.".to_string());
        }
        if self.file_types.is_empty() {
            return Err("Add at least one file type (an extension and its language id).".to_string());
        }
        for (extension, language) in &self.file_types {
            let extension_ok = !extension.is_empty()
                && !extension.starts_with('.')
                && extension.chars().all(|c| c.is_ascii_alphanumeric() || "+_-".contains(c));
            if !extension_ok || language.trim().is_empty() {
                return Err(format!(
                    "File type {extension:?} → {language:?}: an extension without its dot, and a language id."
                ));
            }
        }
        if self.memory_limit.trim().is_empty() || self.cpu_limit.trim().is_empty() {
            return Err("Set a memory and a CPU limit, e.g. 2Gi and 1.".to_string());
        }
        Ok(())
    }
}

/// A catalog lookup's suggested config (SME-35), and what the user should
/// check before saving it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LanguageServerSuggestion {
    pub config: LanguageServerConfig,
    pub notes: Vec<String>,
}

#[cfg(test)]
mod language_server_tests {
    use super::*;

    pub(crate) fn rust_analyzer() -> LanguageServerConfig {
        LanguageServerConfig {
            name: "rust-analyzer".to_string(),
            image: "rust:1".to_string(),
            install_command: "rustup component add rust-analyzer".to_string(),
            command: "rust-analyzer".to_string(),
            file_types: [("rs".to_string(), "rust".to_string())].into(),
            root_markers: vec!["Cargo.toml".to_string()],
            memory_limit: "2Gi".to_string(),
            cpu_limit: "1".to_string(),
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn test_a_complete_config_is_valid() {
        assert_eq!(rust_analyzer().validate(), Ok(()));
    }

    #[test]
    fn test_each_missing_or_malformed_part_is_named() {
        let cases: Vec<(fn(&mut LanguageServerConfig), &str)> = vec![
            (|c| c.name = "Rust Analyzer".into(), "name"),
            (|c| c.name = "-ra".into(), "name"),
            (|c| c.name = "x".repeat(31), "name"),
            (|c| c.image = "rust 1".into(), "image"),
            (|c| c.command = " ".into(), "command"),
            (|c| c.file_types.clear(), "file type"),
            (|c| c.file_types = [(".rs".to_string(), "rust".to_string())].into(), ".rs"),
            (|c| c.memory_limit = String::new(), "memory"),
        ];
        for (break_it, expected) in cases {
            let mut config = rust_analyzer();
            break_it(&mut config);
            let error = config.validate().expect_err("should be refused");
            assert!(error.contains(expected), "{error:?} should mention {expected:?}");
        }
    }

    #[test]
    fn test_a_config_round_trips_through_json() {
        let server = LanguageServer {
            id: 3,
            config: LanguageServerConfig { settings: Some(serde_json::json!({"cargo": {"targetDir": true}})), ..rust_analyzer() },
            updated_at: chrono::Utc::now().naive_utc(),
        };
        let json = serde_json::to_string(&server).expect("serialize");
        assert_eq!(serde_json::from_str::<LanguageServer>(&json).expect("deserialize"), server);
    }
}
