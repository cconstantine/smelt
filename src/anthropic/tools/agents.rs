//! Child agents (SME-236): `start_agent`, offered to a conversation without
//! a parent, and `finish`, offered to an agent. The lifecycle behind them
//! is `crate::agents`.

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
                         message when it calls finish; list_agents shows its status, and the user can \
                         open it and write to it. Up to {} characters.",
                        crate::agents::MAX_BRIEF_CHARS
                    ),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "brief": {"type": "string", "description": "the agent's task, as its first message"}
                        },
                        "required": ["brief"]
                    }),
                },
                run: run!(|c| crate::agents::start_agent(c.pool, c.conversation_id, c.input)),
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
    ]
}
