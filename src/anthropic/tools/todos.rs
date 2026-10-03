//! The conversation's todo list.

use serde_json::Value;
use sqlx::PgPool;

use super::Tool;
use super::TodoItem;
use crate::{db, events};
use crate::anthropic::ToolDefinition;

pub(super) fn tools() -> Vec<Tool> {
    vec![
        Tool {
            def: ToolDefinition {
                name: "todowrite".to_string(),
                description: "Replace this conversation's entire todo list — the model's own \
                               structured tracker for a multi-step plan, visible live to the \
                               user. Always send the complete list, not just what changed: \
                               there are no per-item ids, so a call with 2 todos and a later \
                               call with 3 fully replaces the first with the second, not a \
                               merge. Use to break down and track a non-trivial multi-step \
                               task; skip it for a single simple action."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "todos": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "content": {"type": "string", "description": "what this todo is, non-empty"},
                                    "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                                },
                                "required": ["content", "status"]
                            }
                        }
                    },
                    "required": ["todos"]
                }),
            },
            run: run!(|c| todowrite_tool(c.pool, c.conversation_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "todoread".to_string(),
                description: "Read this conversation's current todo list back, as last set by \
                               todowrite."
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            run: run!(|c| todoread_tool(c.pool, c.conversation_id)),
        },
    ]
}

/// Parses and validates `todowrite`'s `todos` field — a plain list of
/// `{content, status}` objects, no per-item ids (see SME-20's
/// "whole-list replace" decision). Rejects a blank `content` (never a
/// useful todo) before anything is persisted, rather than storing it
/// and letting a later reader be confused by it.
fn parse_todos(input: &Value) -> Result<Vec<TodoItem>, String> {
    let todos = input
        .get("todos")
        .ok_or_else(|| "missing field: todos".to_string())?;
    let todos: Vec<TodoItem> =
        serde_json::from_value(todos.clone()).map_err(|e| format!("invalid todos: {e}"))?;
    if let Some(blank) = todos.iter().position(|t| t.content.trim().is_empty()) {
        return Err(format!("todo at index {blank} has empty content"));
    }
    Ok(todos)
}

pub(super) async fn todowrite_tool(
    pool: &PgPool,
    conversation_id: i64,
    input: &Value,
) -> Result<String, String> {
    let todos = parse_todos(input)?;
    db::set_conversation_todos(pool, conversation_id, &todos)
        .await
        .map_err(|e| e.to_string())?;
    events::publish(
        conversation_id,
        events::ConversationEvent::TodoListUpdate {
            items: todos.clone(),
        },
    );
    serde_json::to_string(&todos).map_err(|e| e.to_string())
}

pub(super) async fn todoread_tool(pool: &PgPool, conversation_id: i64) -> Result<String, String> {
    let todos = db::get_conversation_todos(pool, conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string(&todos).map_err(|e| e.to_string())
}
