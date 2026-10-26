//! The server tool dispatch's tests: shared fixtures here, the tests in
//! topic files by subject (SME-241 split the one file it outgrew).

use super::*;
use super::super::{TodoItem, TodoStatus, files::*, git::*, pods::*, todos::*, web::*};
use crate::anthropic::ContentBlock;
use crate::models::Message;
use crate::db;

fn test_pool() -> PgPool {
    // Never actually connected to: these tests only reach tools that
    // fail on their input before touching the pool. `PgPool::
    // connect_lazy` doesn't dial out until first use, so a bogus URL
    // is safe to construct here.
    PgPool::connect_lazy("postgres://unused:unused@localhost/unused")
        .expect("lazy pool construction never fails")
}

fn message_with_blocks(id: i64, blocks: Vec<ContentBlock>) -> Message {
    Message {
        id,
        conversation_id: 1,
        role: "assistant".to_string(),
        content: serde_json::to_string(&blocks).expect("ContentBlock always serializes"),
        created_at: chrono::Utc::now().naive_utc(),
    }
}

fn tool_use(id: &str, name: &str, input: Value) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.to_string(),
        name: name.to_string(),
        input,
    }
}

fn tool_result(tool_use_id: &str, content: Value, is_error: bool) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: content.to_string(),
        is_error: if is_error { Some(true) } else { None },
    }
}

fn span_attribute<'a>(
    span: &'a opentelemetry_sdk::trace::SpanData,
    key: &str,
) -> Option<&'a opentelemetry::Value> {
    let values: Vec<_> =
        span.attributes.iter().filter(|kv| kv.key.as_str() == key).map(|kv| &kv.value).collect();
    assert!(values.len() <= 1, "{key} recorded more than once: {values:?}");
    values.first().copied()
}


mod caps;
mod dispatch;
mod files;
mod git;
mod pods;
mod previews;
mod rules;
mod spans;
mod terminals;
mod todos;
