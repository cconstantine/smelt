//! `compact_context` (SME-482): the model asks smelt to compact its own
//! conversation now, to save tokens. The call only records the request
//! (`turn::request_compaction`); the turn compacts at the end of the
//! call's tool pass, at most once a turn (`turn::on_request`).

use super::{Class, Offer, Registered, Tool};
use crate::anthropic::ToolDefinition;

pub(crate) const COMPACT_CONTEXT: &str = "compact_context";

/// The longest note, in characters, once trimmed.
pub(crate) const MAX_NOTE_CHARS: usize = 2_000;

pub(super) fn tools() -> Vec<Registered> {
    vec![Registered {
        offer: Offer::All,
        tool: Tool {
            def: ToolDefinition {
                name: COMPACT_CONTEXT.to_string(),
                description: "smelt: compact this conversation's context now, to save tokens: smelt summarises \
                              the history and you continue from the summary, with the user's and parent's own \
                              messages and your todo list kept word for word. Use it at a natural break (a task \
                              finished, before starting an unrelated one), not mid-task. Before calling it, record \
                              what you'll need in a durable place (your todo list, the ticket); the summary is a \
                              narration, not a record. Call it on its own: other calls' results in the same reply \
                              are summarised before you read them. Once per turn; `note` says what the summary \
                              must keep."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "note": {
                            "type": "string",
                            "description": format!("optional, up to {MAX_NOTE_CHARS} characters: what the summary must keep")
                        }
                    }
                }),
            },
            class: Class::READ_ONLY,
            run: run!(|c| compact(c.conversation_id, c.input)),
        },
    }]
}

/// A compaction's optional note, trimmed: `None` when absent or blank, and
/// refused over `MAX_NOTE_CHARS` characters or when it holds a character
/// `git::not_plain` matches other than a newline (SME-276's family: no
/// control, bidi or invisible character). The tool and the endpoint share it.
pub(crate) fn parse_note(note: Option<&str>) -> Result<Option<String>, String> {
    let note = note.unwrap_or_default().trim();
    if note.is_empty() {
        return Ok(None);
    }
    let rule = format!("note must be up to {MAX_NOTE_CHARS} characters, with no control or invisible characters but newlines");
    let chars = note.chars().count();
    if chars > MAX_NOTE_CHARS {
        return Err(format!("{rule} (it has {chars} characters)"));
    }
    if let Some(at) = note.chars().position(|c| c != '\n' && crate::git::not_plain(c)) {
        return Err(format!("{rule} (it holds one at character {})", at + 1));
    }
    Ok(Some(note.to_string()))
}

/// Records the request; the turn compacts once this tool pass ends.
async fn compact(conversation_id: i64, input: &serde_json::Value) -> Result<String, String> {
    let note = parse_note(input.get("note").and_then(serde_json::Value::as_str))?;
    crate::turn::request_compaction(conversation_id, note)?;
    Ok("This conversation will be compacted once this tool pass ends, before your next model call: you'll go on \
        from a summary, with the user's and parent's messages and your todo list kept word for word."
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T21: what's refused, each with the rule in its words, and what's
    /// kept: newlines, and a blank note is none.
    #[test]
    fn test_parse_note_keeps_up_to_2000_plain_characters_and_newlines() {
        let rule = "note must be up to 2000 characters, with no control or invisible characters but newlines";
        assert_eq!(parse_note(Some(&"x".repeat(2_001))), Err(format!("{rule} (it has 2001 characters)")));
        assert_eq!(parse_note(Some("keep\u{1b}[31m")), Err(format!("{rule} (it holds one at character 5)")));
        assert_eq!(parse_note(Some("a\u{200b}")), Err(format!("{rule} (it holds one at character 2)")));
        assert_eq!(parse_note(Some("keep\nthe SHA")), Ok(Some("keep\nthe SHA".to_string())), "newlines kept");
        assert_eq!(parse_note(Some(&"é".repeat(2_000))), Ok(Some("é".repeat(2_000))), "characters, not bytes");
        assert_eq!(parse_note(Some("  \n\t ")), Ok(None), "whitespace only is none");
        assert_eq!(parse_note(None), Ok(None));
        assert_eq!(parse_note(Some("  keep it  ")), Ok(Some("keep it".to_string())), "trimmed");
    }

    /// T17: offered to every conversation (top-level, child and linked),
    /// and every persona the repo has lets it through at its own class.
    #[test]
    fn test_compact_context_is_offered_to_every_conversation_and_persona() {
        use crate::personas::rules::{RuleChain, ToolRef};
        let registered = tools().pop().expect("the tool");
        assert_eq!(registered.offer, Offer::All);
        assert!(
            crate::anthropic::tools::native_tool_definitions().iter().any(|d| d.name == COMPACT_CONTEXT),
            "in the registry"
        );
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".agents/personas");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("the persona dir") {
            let path = entry.expect("an entry").path();
            if path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read the persona");
            let shown = path.display().to_string();
            let persona = crate::personas::parse(&shown, &text, text.len() as u64).expect("a valid persona");
            let stored = persona.tools.map(|rules| serde_json::to_value(vec![rules]).expect("rules serialize"));
            let chain = RuleChain::try_from_stored(stored).expect("a chain");
            assert!(
                chain.allows_any(ToolRef::Native(COMPACT_CONTEXT), registered.tool.class.can_be()),
                "{shown} lets compact_context through"
            );
            checked += 1;
        }
        assert!(checked >= 7, "found the repo's personas ({checked})");
    }
}
