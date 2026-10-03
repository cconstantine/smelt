//! A live browsing session.

use serde_json::Value;
use sqlx::PgPool;

use super::Tool;
use super::server::{required_i64, required_str};
use crate::anthropic::ToolDefinition;

pub(super) fn tools() -> Vec<Tool> {
    vec![
        Tool {
            def: ToolDefinition {
                name: "open_browser_session".to_string(),
                description: "Open a persistent, interactive browsing session for this \
                               conversation — a real browser page that stays open across \
                               multiple browser_navigate/browser_click/browser_fill/browser_back \
                               calls, unlike webfetch's one-shot fetch. At most one session per \
                               conversation; refuses if one is already open (close_browser_session \
                               first). The user can watch and interact with this session live too."
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            run: run!(|c| open_browser_session_tool(c.pool, c.conversation_id)),
        },
        Tool {
            def: ToolDefinition {
                name: "close_browser_session".to_string(),
                description: "Close this conversation's browsing session. A no-op if none is open."
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            run: run!(|c| close_browser_session_tool(c.conversation_id)),
        },
        Tool {
            def: ToolDefinition {
                name: "browser_navigate".to_string(),
                description: "Navigate the open browsing session to a URL and return the \
                               resulting page's readable text plus a list of interactive \
                               elements (each with an index — pass that index to browser_click/ \
                               browser_fill). Requires open_browser_session first. http/https \
                               only, same address rules as webfetch: localhost (or 127.0.0.1) \
                               means this conversation's sandbox pod."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "the http:// or https:// URL to navigate to"}
                    },
                    "required": ["url"]
                }),
            },
            run: run!(|c| browser_navigate_tool(c.conversation_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "browser_click".to_string(),
                description: "Click the element at the given index (from the most recent \
                               browser_navigate/browser_click/browser_fill/browser_back/ \
                               browser_read response's elements list — an index from an older \
                               response is meaningless) and return the resulting page's state. \
                               May or may not navigate, depending on the element."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "element_index": {"type": "integer", "description": "an index from the most recent response's elements list"}
                    },
                    "required": ["element_index"]
                }),
            },
            run: run!(|c| browser_click_tool(c.conversation_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "browser_fill".to_string(),
                description: "Type a value into the element at the given index (an input, \
                               textarea, ...), replacing whatever it already contained (an \
                               empty value clears it), and return the resulting page's state. \
                               Does not submit — call browser_click on a submit control \
                               separately."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "element_index": {"type": "integer", "description": "an index from the most recent response's elements list"},
                        "value": {"type": "string"}
                    },
                    "required": ["element_index", "value"]
                }),
            },
            run: run!(|c| browser_fill_tool(c.conversation_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "browser_back".to_string(),
                description: "Navigate the open browsing session back in its history and \
                               return the resulting page's state."
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            run: run!(|c| browser_back_tool(c.conversation_id)),
        },
        Tool {
            def: ToolDefinition {
                name: "browser_read".to_string(),
                description: "Re-read the open browsing session's current page — its readable \
                               text and a fresh interactive-element list — without taking any \
                               action. Useful after the user interacts with the live panel \
                               themselves."
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            run: run!(|c| browser_read_tool(c.conversation_id)),
        },
    ]
}

async fn open_browser_session_tool(pool: &PgPool, conversation_id: i64) -> Result<String, String> {
    let sandbox = crate::egress_proxy::sandbox_dial(pool.clone(), conversation_id);
    crate::browsing::open_session(conversation_id, sandbox).await?;
    Ok("browser session opened".to_string())
}

async fn close_browser_session_tool(conversation_id: i64) -> Result<String, String> {
    crate::browsing::close_session(conversation_id).await?;
    Ok("browser session closed".to_string())
}

async fn browser_navigate_tool(conversation_id: i64, input: &Value) -> Result<String, String> {
    let url = required_str(input, "url")?;
    let state = crate::browsing::navigate(conversation_id, &url).await?;
    serde_json::to_string(&state).map_err(|e| e.to_string())
}

async fn browser_click_tool(conversation_id: i64, input: &Value) -> Result<String, String> {
    let element_index = required_i64(input, "element_index")?;
    let state = crate::browsing::click(conversation_id, element_index as usize).await?;
    serde_json::to_string(&state).map_err(|e| e.to_string())
}

async fn browser_fill_tool(conversation_id: i64, input: &Value) -> Result<String, String> {
    let element_index = required_i64(input, "element_index")?;
    let value = required_str(input, "value")?;
    let state = crate::browsing::fill(conversation_id, element_index as usize, &value).await?;
    serde_json::to_string(&state).map_err(|e| e.to_string())
}

async fn browser_back_tool(conversation_id: i64) -> Result<String, String> {
    let state = crate::browsing::go_back(conversation_id).await?;
    serde_json::to_string(&state).map_err(|e| e.to_string())
}

async fn browser_read_tool(conversation_id: i64) -> Result<String, String> {
    let state = crate::browsing::read(conversation_id).await?;
    serde_json::to_string(&state).map_err(|e| e.to_string())
}
