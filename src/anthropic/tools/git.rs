//! Repos cloned into the sandbox, their AGENTS.md and their personas.

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
        Tool {
            def: ToolDefinition {
                name: "list_personas".to_string(),
                description: "List the personas in this conversation's repos: each repo's \
                               .agents/personas/*.md files, read from its checkout now. A \
                               persona is named <repo dir>:<name> (smelt:developer), with its \
                               description; a file that doesn't parse is listed with its error. \
                               Only a repo the user trusts shows its personas; for one they \
                               haven't decided about, it says how they can be asked. Nothing \
                               can be started with a persona yet."
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            run: run!(|c| crate::git::list_personas(c.pool, c.conversation_id)),
        },
    ]
}

async fn clone_repo_tool(pool: &PgPool, conversation_id: i64, input: &Value) -> Result<String, String> {
    let url = required_str(input, "url")?;
    let optional = |field: &str| input.get(field).and_then(Value::as_str).map(str::to_string);
    let branch = optional("branch");
    let dir = optional("dir");
    let repo = crate::git::clone_repo(pool, conversation_id, &url, branch.as_deref(), dir.as_deref()).await?;
    let persona_files = crate::git::repo_persona_files(pool, repo.id).await;
    Ok(clone_result_for_model(repo, &persona_files))
}

/// What the model is told about a clone: the repo summary without the
/// trust card's preview, and its persona files' names. An AGENTS.md the
/// user hasn't trusted must not reach the model's context, which is the
/// point of asking; nor may a persona file's content.
pub(super) fn clone_result_for_model(mut repo: crate::git::RepoSummary, persona_files: &[String]) -> String {
    repo.trust_requests.clear();
    let mut told = serde_json::to_value(&repo).unwrap_or_default();
    let mut notes = Vec::new();
    // SME-272: the checked-out branch is the remote owner's choice, and
    // serde_json leaves a line separator or bidi control in it raw.
    if repo.branch.as_deref().is_some_and(|b| !crate::git::plain_path_text(b)) {
        told["branch"] = Value::Null;
        notes.push("Its checked-out branch isn't shown: the name holds control characters.".to_string());
    }
    if !repo.agents_files.is_empty() {
        notes.push(format!(
            "This repo has AGENTS.md files (agents_files, relative to {}). Load the ones for \
             the code you'll work on with load_instructions: the top-level one, and the \
             nearest one to the files you change.",
            repo.path
        ));
    }
    if !persona_files.is_empty() {
        told["persona_files"] = serde_json::json!(persona_files);
        notes.push(
            "It has persona files (persona_files, relative to it): list_personas shows them once \
             the user trusts the repo."
                .to_string(),
        );
    }
    if !notes.is_empty() {
        told["note"] = Value::String(notes.join(" "));
    }
    told.to_string()
}

async fn load_instructions_tool(pool: &PgPool, conversation_id: i64, input: &Value) -> Result<String, String> {
    let path = required_str(input, "path")?;
    crate::git::load_instructions(pool, conversation_id, &path).await
}

#[cfg(test)]
mod tests;
