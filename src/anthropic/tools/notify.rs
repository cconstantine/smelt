//! `notify_user` (SME-377): the model tells the user it's done, or that
//! something needs them, with one line the desktop notification shows.
//! It replaced the automatic "done" every turn's end used to note: a
//! model waiting on its own commands ended its turn at each step, and
//! each end buzzed. The call notes `Attention::Notified` for the busy
//! spell (`turn::note_notified`), the spell's end publishes it, and the
//! turn ends here, as `finish`'s does (`turn::reply::run_tools`). Offered
//! only to a conversation the user started: an agent reports with
//! `finish`.

use super::{Class, Offer, Registered, Tool};
use crate::anthropic::ToolDefinition;

pub(crate) const NOTIFY_USER: &str = "notify_user";

/// The longest message, in characters, once trimmed: about two lines of
/// a macOS, Windows or GNOME banner.
pub(crate) const MAX_NOTIFY_CHARS: usize = 200;

pub(super) fn tools() -> Vec<Registered> {
    vec![Registered {
        offer: Offer::TopLevel,
        tool: Tool {
            def: ToolDefinition {
                name: NOTIFY_USER.to_string(),
                description: "smelt: send the user a desktop notification saying `message`, once you've \
                              finished what they asked or need them to look at something (a result, a \
                              preview, a problem you can't get past). Call it once, as your last step: your \
                              turn ends here, and the notification shows when this conversation goes idle. \
                              Don't call it while you wait on your own commands, their notices or your \
                              agents (their messages wake you), and don't use it to ask anything: ask_user \
                              notifies by itself. It's a notification in the user's browser, not a message \
                              on any MCP server's chat or tracker."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message": {
                            "type": "string",
                            "description": format!("one line, up to {MAX_NOTIFY_CHARS} characters: what's done, or what needs the user")
                        }
                    },
                    "required": ["message"]
                }),
            },
            class: Class::READ_ONLY,
            run: run!(|c| notify(c.conversation_id, c.input)),
        },
    }]
}

/// The call's `message`, trimmed: one line of 1 to `MAX_NOTIFY_CHARS`
/// characters with nothing `git::not_plain` matches (SME-276's rule: no
/// control, separator, bidi or invisible character, so no newline
/// either). Refused rather than escaped, telling the model what to fix.
pub(crate) fn parse_notify_message(input: &serde_json::Value) -> Result<String, String> {
    let message = input.get("message").and_then(serde_json::Value::as_str).unwrap_or_default().trim();
    let rule = format!(
        "message must be one line of 1 to {MAX_NOTIFY_CHARS} characters, with no control or invisible characters"
    );
    let chars = message.chars().count();
    if chars == 0 || chars > MAX_NOTIFY_CHARS {
        return Err(format!("{rule} (it has {chars} characters)"));
    }
    if let Some(at) = message.chars().position(crate::git::not_plain) {
        return Err(format!("{rule} (it holds one at character {})", at + 1));
    }
    Ok(message.to_string())
}

/// Notes the message for the spell's end; the turn ends on the result.
async fn notify(conversation_id: i64, input: &serde_json::Value) -> Result<String, String> {
    let message = parse_notify_message(input)?;
    crate::turn::note_notified(conversation_id, message);
    Ok("The user will be notified when this conversation goes idle.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(message: &str) -> Result<String, String> {
        parse_notify_message(&serde_json::json!({ "message": message }))
    }

    /// T8: what's refused, each with the rule in its words, and what's
    /// kept: exactly the limit, and a padded message stored trimmed.
    #[test]
    fn test_parse_notify_message_keeps_one_plain_line_of_up_to_200_characters() {
        let rule = "message must be one line of 1 to 200 characters, with no control or invisible characters";
        for (message, why) in [
            ("", "(it has 0 characters)"),
            ("   \t ", "(it has 0 characters)"),
            (&"x".repeat(201), "(it has 201 characters)"),
            ("built\nand tested", "(it holds one at character 6)"),
            ("built\u{200b}", "(it holds one at character 6)"),
            ("ok \u{202e}txt.exe", "(it holds one at character 4)"),
            ("\u{1b}[31mred", "(it holds one at character 1)"),
        ] {
            assert_eq!(parse(message), Err(format!("{rule} {why}")), "{message:?}");
        }
        let longest = "é".repeat(200);
        assert_eq!(parse(&longest), Ok(longest.clone()), "200 characters, not bytes");
        assert_eq!(parse("  Built and tested \n"), Ok("Built and tested".to_string()), "stored trimmed");
        assert_eq!(
            parse_notify_message(&serde_json::json!({})),
            Err(format!("{rule} (it has 0 characters)")),
            "a missing message is an empty one"
        );
    }
}
