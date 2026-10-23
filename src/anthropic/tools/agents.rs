//! Child agents (SME-236): `start_agent`, offered to a conversation without
//! a parent, `list_agents` and `stop_agent`, offered to every conversation,
//! and `finish`, offered to an agent; and (SME-238) `message_agent`, offered
//! to every conversation but callable only by one with a parent or agents
//! of its own, and `ask_parent`, offered to an agent. The lifecycle behind them is `crate::agents`.

use super::{Offer, Registered, Tool};
use crate::anthropic::ToolDefinition;

pub(super) fn tools() -> Vec<Registered> {
    vec![
        Registered {
            offer: Offer::Root,
            tool: Tool {
                def: ToolDefinition {
                    name: "start_agent".to_string(),
                    description: format!(
                        "Start a child agent: a new conversation of its own, with its own sandbox, \
                         that works on `brief` in the background while you go on. Returns its id at \
                         once. The brief is its first message, so say everything it needs: the task, \
                         what to report back, and when it's done. Its report arrives here as a \
                         message when it calls finish; message_agent writes to it, list_agents shows \
                         its status, and the user can open it and write to it. Up to {} characters.",
                        crate::agents::MAX_BRIEF_CHARS
                    ),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "brief": {"type": "string", "description": "the agent's task, as its first message"},
                            "repo": {
                                "type": "string",
                                "description": "one of your checkouts, /workspace/<dir>: the agent gets its own clone of it, with its AGENTS.md loaded (its remote must be trusted)"
                            },
                            "ref": {
                                "type": "string",
                                "description": "the branch, tag or commit id of repo's remote to check out, as the remote names it (main, v1.2, a commit id; not origin/main or HEAD~1); without it, the commit your checkout is on, which must be pushed and clean"
                            },
                            "persona": {
                                "type": "string",
                                "description": "a persona of repo (list_personas lists them), by name: the agent runs under its instructions, read at its ref"
                            }
                        },
                        "required": ["brief"]
                    }),
                },
                run: run!(|c| crate::agents::start_agent(c.pool, c.conversation_id, c.input)),
            },
        },
        Registered {
            offer: Offer::All,
            tool: Tool {
                def: ToolDefinition {
                    name: "list_agents".to_string(),
                    description: "List the agents you started (never anyone else's), newest first: \
                                  id, title, persona, status (starting, running, waiting for the user, \
                                  waiting for its parent, idle, finished, stopped or failed), when it \
                                  started, its last \
                                  message, and how many agents it started itself."
                        .to_string(),
                    input_schema: serde_json::json!({"type": "object", "properties": {}}),
                },
                run: run!(|c| crate::agents::list_agents(c.pool, c.conversation_id)),
            },
        },
        Registered {
            offer: Offer::All,
            tool: Tool {
                def: ToolDefinition {
                    name: "stop_agent".to_string(),
                    description: "Stop one of the agents you started, and every agent under it: \
                                  their turns end and they take no more work unless the user writes \
                                  to them. Their sandboxes are left as they are. Only your own agents' \
                                  ids (list_agents lists them)."
                        .to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {"id": {"type": "integer", "description": "the agent's id"}},
                        "required": ["id"]
                    }),
                },
                run: run!(|c| crate::agents::stop_agent(c.pool, c.conversation_id, c.input)),
            },
        },
        Registered {
            offer: Offer::Child,
            tool: Tool {
                def: ToolDefinition {
                    name: crate::agents::FINISH.to_string(),
                    description: format!(
                        "Finish your work as an agent: `report` goes to your parent, the conversation \
                         that started you, as your final report, and your turn ends here. Say what you \
                         did, what you found and anything left undone; your parent reads only this. \
                         Up to {} characters. The user may still write to you afterwards.",
                        crate::agents::MAX_REPORT_CHARS
                    ),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "report": {"type": "string", "description": "your final report to your parent"}
                        },
                        "required": ["report"]
                    }),
                },
                run: run!(|c| crate::agents::finish(c.pool, c.conversation_id, c.input)),
            },
        },
        Registered {
            offer: Offer::Linked,
            tool: Tool {
                def: ToolDefinition {
                    name: "message_agent".to_string(),
                    description: format!(
                        "Send a message to your parent (to: \"parent\") or to one of the agents you \
                         started (to: its id), at any time. It arrives in their conversation as a message \
                         from you: an idle one is woken to read it, a busy one reads it after its current \
                         step. Not to anyone else, nor to an agent that is stopped or failed. Up to {} \
                         characters.",
                        crate::agents::MAX_MESSAGE_CHARS
                    ),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "to": {"type": "string", "description": "\"parent\", or the id of one of your agents"},
                            "text": {"type": "string", "description": "the message"}
                        },
                        "required": ["to", "text"]
                    }),
                },
                run: run!(|c| crate::agents::message_agent(c.pool, c.conversation_id, c.input)),
            },
        },
        Registered {
            offer: Offer::Child,
            tool: Tool {
                def: ToolDefinition {
                    name: crate::agents::ASK_PARENT.to_string(),
                    description: format!(
                        "Ask your parent, the conversation that started you, a question and wait for \
                         the answer: your turn ends here, and your parent's next message_agent to you \
                         after it has read your question is the answer, which wakes you. If the user writes to you first, their message \
                         comes instead, and a later reply from your parent arrives as a message. One \
                         question per reply, and not with ask_user. Up to {} characters.",
                        crate::agents::MAX_MESSAGE_CHARS
                    ),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {"question": {"type": "string", "description": "the question, with what your parent needs to answer it"}},
                        "required": ["question"]
                    }),
                },
                // The turn handles it (`turn::run_tools`), as `ask_user`.
                run: run!(|_c| async {
                    Err::<String, String>(format!("{} is answered by your parent, not run", crate::agents::ASK_PARENT))
                }),
            },
        },
    ]
}
