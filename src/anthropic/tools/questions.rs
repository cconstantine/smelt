//! `ask_user` (SME-34): the model asks the user one to four questions.
//! The turn loop handles the call itself (`turn::questions`): it ends the
//! turn and the conversation waits for the answer, which becomes the
//! call's result. This module only offers the definition.

use super::Tool;
use crate::anthropic::ToolDefinition;
use crate::questions::{ASK_USER, MAX_HEADER_CHARS, MAX_OPTIONS, MAX_QUESTIONS, MIN_OPTIONS};

pub(super) fn tools() -> Vec<Tool> {
    vec![Tool {
        def: ToolDefinition {
            name: ASK_USER.to_string(),
            description: format!(
                "Ask the user one to {MAX_QUESTIONS} questions and wait for their answer, when the \
                 answer changes what you do next (which of two approaches, whether to delete or \
                 overwrite something, a choice only they can make). Your turn ends here: the user \
                 answers on a card in the chat, maybe much later, and their answer comes back as \
                 this call's result. Offer {MIN_OPTIONS} to {MAX_OPTIONS} options when the choices \
                 are clear (put a recommended one first, ending its label with \"(Recommended)\"), \
                 or none for a free-text answer. The user can always write their own answer \
                 instead, so don't add an \"Other\" option. Ask everything you need in one call; \
                 don't use this for rhetorical or conversational questions, which plain text \
                 suits."
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_QUESTIONS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {"type": "string", "description": "the full question"},
                                "header": {
                                    "type": "string",
                                    "description": format!("a short label for it, up to {MAX_HEADER_CHARS} characters")
                                },
                                "options": {
                                    "type": "array",
                                    "description": format!("{MIN_OPTIONS} to {MAX_OPTIONS} choices with unique labels, or none for free text"),
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {"type": "string"},
                                            "description": {"type": "string", "description": "what choosing it means"}
                                        },
                                        "required": ["label"]
                                    }
                                },
                                "multi_select": {
                                    "type": "boolean",
                                    "description": "let the user choose several options (default false)"
                                }
                            },
                            "required": ["question", "header"]
                        }
                    }
                },
                "required": ["questions"]
            }),
        },
        run: run!(|_c| async { Err::<String, String>(format!("{ASK_USER} is answered by the user, not run")) }),
    }]
}
