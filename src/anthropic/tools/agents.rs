//! Child agents (SME-236): `start_agent`, offered to a conversation without
//! a parent. The lifecycle behind them is `crate::agents`.

use super::{Offer, Registered, Tool};
use crate::anthropic::ToolDefinition;

pub(super) fn tools() -> Vec<Registered> {
    vec![Registered {
        offer: Offer::Root,
        tool: Tool {
            def: ToolDefinition {
                name: "start_agent".to_string(),
                description: format!(
                    "Start a child agent: a new conversation of its own, with its own sandbox, \
                     that works on `brief` in the background while you go on. Returns its id at \
                     once. The brief is its first message, so say everything it needs: the task, \
                     what to report back, and when it's done. Its report arrives here as a message \
                     when it finishes; list_agents shows its status, and the user can open it and \
                     write to it. Up to {} characters.",
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
    }]
}
