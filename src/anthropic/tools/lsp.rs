//! Language servers in the sandbox.

use serde_json::Value;
use sqlx::PgPool;

use super::Tool;
use super::server::required_str;
use crate::sandbox;
use crate::anthropic::ToolDefinition;

pub(super) fn tools() -> Vec<Tool> {
    vec![
        // --- Language servers, next to the sandbox pod — see SME-35.
        Tool {
            def: ToolDefinition {
                name: "lsp_servers".to_string(),
                description: "List the language servers the user has configured, the file \
                               types each takes, and whether each is running for this \
                               conversation's sandbox."
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            run: run!(|c| lsp_servers_tool(c.pool, c.conversation_id)),
        },
        Tool {
            def: ToolDefinition {
                name: "start_language_server".to_string(),
                description: "Start a configured language server next to this conversation's \
                               sandbox pod (create_pod first). The first start installs it, \
                               which can take a few minutes. Once it runs, use the lsp tool on \
                               its files, and edit_file/write_file results include its errors \
                               and warnings for the file. Starting a running server whose \
                               settings changed restarts it."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"name": {"type": "string", "description": "as lsp_servers lists it"}},
                    "required": ["name"]
                }),
            },
            run: run!(|c| start_language_server_tool(c.pool, c.conversation_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "lsp".to_string(),
                description: "Ask the running language server for a file (see lsp_servers) \
                               about code in the sandbox: definition, references, \
                               implementation, hover, incoming_calls and outgoing_calls need \
                               path, line and character; document_symbols and diagnostics need \
                               path; workspace_symbols takes a query; rename needs path, line, \
                               character and new_name, and edits the files itself. Lines and \
                               characters are 1-based, as read_file shows them."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "operation": {"type": "string", "enum": crate::lsp::ops::Operation::ALL.iter().map(|op| op.name()).collect::<Vec<_>>()},
                        "path": {"type": "string"},
                        "line": {"type": "integer", "minimum": 1},
                        "character": {"type": "integer", "minimum": 1, "description": "defaults to 1"},
                        "query": {"type": "string"},
                        "new_name": {"type": "string"}
                    },
                    "required": ["operation"]
                }),
            },
            run: run!(|c| lsp_tool(c.pool, c.conversation_id, c.input)),
        },
    ]
}

async fn lsp_servers_tool(pool: &PgPool, conversation_id: i64) -> Result<String, String> {
    let client = sandbox::kube_client().map_err(|e| e.to_string())?;
    let sandbox = crate::lsp::manager::sandbox_ref(pool, &client, conversation_id).await.ok();
    crate::lsp::manager::servers_summary(pool, &client, sandbox.as_ref()).await
}

async fn start_language_server_tool(
    pool: &PgPool,
    conversation_id: i64,
    input: &Value,
) -> Result<String, String> {
    let name = required_str(input, "name")?;
    let client = sandbox::kube_client().map_err(|e| e.to_string())?;
    let sandbox = crate::lsp::manager::sandbox_ref(pool, &client, conversation_id).await?;
    crate::lsp::manager::start(pool, &client, &sandbox, &name).await
}

async fn lsp_tool(pool: &PgPool, conversation_id: i64, input: &Value) -> Result<String, String> {
    let operation = crate::lsp::ops::Operation::parse(&required_str(input, "operation")?)?;
    let text = |field: &str| input.get(field).and_then(Value::as_str).map(str::to_string);
    let number = |field: &str| input.get(field).and_then(Value::as_u64).map(|n| n as u32);
    let request = crate::lsp::manager::OperationInput {
        path: text("path"),
        line: number("line"),
        character: number("character"),
        query: text("query"),
        new_name: text("new_name"),
    };
    let client = sandbox::kube_client().map_err(|e| e.to_string())?;
    let sandbox = crate::lsp::manager::sandbox_ref(pool, &client, conversation_id).await?;
    crate::lsp::manager::operate(pool, &client, &sandbox, operation, &request).await
}
