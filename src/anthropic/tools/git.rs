//! Repos cloned into the sandbox, and their AGENTS.md.

use serde_json::Value;
use sqlx::PgPool;

use super::Tool;
use super::server::required_str;
use crate::anthropic::ToolDefinition;

pub(super) fn tools() -> Vec<Tool> {
    vec![
        Tool {
            def: ToolDefinition {
                name: "clone_repo".to_string(),
                description: "Clone a git repository into this conversation's sandbox pod, \
                               at /workspace/<dir> (the repo's name by default). Use this \
                               rather than `git clone` in a terminal. Starts the sandbox if \
                               the conversation has none. Takes an SSH URL (git@github.com:owner/repo.git) or an \
                               https one; https only works for public repos, and pushing \
                               needs SSH. Returns where it is and what was checked out."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "the repository's URL"},
                        "branch": {"type": "string", "description": "branch or tag to check out; the remote's default if omitted"},
                        "dir": {"type": "string", "description": "directory name under /workspace; the repo's name if omitted"}
                    },
                    "required": ["url"]
                }),
            },
            run: run!(|c| clone_repo_tool(c.pool, c.conversation_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "load_instructions".to_string(),
                description: "Load a repository's AGENTS.md into your instructions: it's added \
                               to the system prompt's Project instructions and stays there on \
                               every turn. Load the top-level one of a repo you work on, and \
                               the nearest one to the files you change (nearest wins). Call \
                               again after the file changes to load its new version. Only a \
                               repo the user trusts loads; for one they haven't decided about, \
                               they're asked, and you get a message when they decide."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "the AGENTS.md file, e.g. /workspace/smelt/AGENTS.md or /workspace/smelt/web/AGENTS.md"}
                    },
                    "required": ["path"]
                }),
            },
            run: run!(|c| load_instructions_tool(c.pool, c.conversation_id, c.input)),
        },
    ]
}

async fn clone_repo_tool(pool: &PgPool, conversation_id: i64, input: &Value) -> Result<String, String> {
    let url = required_str(input, "url")?;
    let optional = |field: &str| input.get(field).and_then(Value::as_str).map(str::to_string);
    let branch = optional("branch");
    let dir = optional("dir");
    let repo = crate::git::clone_repo(pool, conversation_id, &url, branch.as_deref(), dir.as_deref()).await?;
    Ok(clone_result_for_model(repo))
}

/// What the model is told about a clone: the repo summary without the
/// trust card's preview. An AGENTS.md the user hasn't trusted must not
/// reach the model's context, which is the point of asking.
pub(super) fn clone_result_for_model(mut repo: crate::git::RepoSummary) -> String {
    repo.trust_requests.clear();
    let mut told = serde_json::to_value(&repo).unwrap_or_default();
    if !repo.agents_files.is_empty() {
        told["note"] = Value::String(format!(
            "This repo has AGENTS.md files (agents_files, relative to {}). Load the ones for \
             the code you'll work on with load_instructions: the top-level one, and the \
             nearest one to the files you change.",
            repo.path
        ));
    }
    told.to_string()
}

async fn load_instructions_tool(pool: &PgPool, conversation_id: i64, input: &Value) -> Result<String, String> {
    let path = required_str(input, "path")?;
    crate::git::load_instructions(pool, conversation_id, &path).await
}
