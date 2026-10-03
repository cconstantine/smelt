//! What a request sends: the history, rebuilt from the saved messages,
//! and the system prompt.

use super::*;

/// Builds the message history actually replayed to Anthropic — every
/// persisted message, *except* the compaction boundary, if any, changes
/// what's replayed without anything having been rewritten or deleted in
/// storage. Finds the *latest* `CompactionSummary` block across every
/// message (a long conversation can compact more than once; only the most
/// recent boundary matters — everything at or before it, including any
/// earlier compaction's own summary message, is superseded), skips every
/// message at or before that boundary, and translates the summary itself
/// from `CompactionSummary` into a plain `Text` block — Anthropic has no
/// concept of the former. See SME-18.
///
/// Thinking blocks in messages up to `thinking_stripped_through` were
/// signed by another provider or model, so they're left out (SME-72).
#[cfg(feature = "server")]
pub(super) fn history_for_request(
    messages: Vec<Message>,
    thinking_stripped_through: Option<i64>,
) -> Result<Vec<anthropic::AnthropicMessage>, serde_json::Error> {
    let parsed = messages
        .into_iter()
        .map(|m| {
            let blocks = m.blocks()?;
            Ok((m.id, m.role, blocks))
        })
        .collect::<Result<Vec<(i64, String, Vec<anthropic::ContentBlock>)>, serde_json::Error>>()?;

    let latest_boundary = parsed
        .iter()
        .flat_map(|(_, _, blocks)| blocks.iter())
        .filter_map(|block| match block {
            anthropic::ContentBlock::CompactionSummary {
                covers_through_message_id,
                ..
            } => Some(*covers_through_message_id),
            _ => None,
        })
        .max();

    let history = parsed
        .into_iter()
        .filter(|(id, _, _)| latest_boundary.is_none_or(|boundary| *id > boundary))
        .map(|(id, role, blocks)| {
            let strip_thinking = thinking_stripped_through.is_some_and(|through| id <= through);
            let content = strip_foreign_thinking(blocks, strip_thinking)
                .into_iter()
                .map(|block| match block {
                    anthropic::ContentBlock::CompactionSummary { summary, .. } => {
                        anthropic::ContentBlock::Text { text: summary }
                    }
                    anthropic::ContentBlock::CompactionPlaceholder { text } => {
                        anthropic::ContentBlock::Text { text }
                    }
                    other => other,
                })
                .collect();
            anthropic::AnthropicMessage { role, content }
        })
        .collect();
    Ok(answer_unfinished_tool_calls(history))
}

/// `blocks` without their thinking blocks when `strip` is set (see
/// `anthropic::types::strip_thinking`).
#[cfg(feature = "server")]
pub(super) fn strip_foreign_thinking(blocks: Vec<anthropic::ContentBlock>, strip: bool) -> Vec<anthropic::ContentBlock> {
    if strip { anthropic::types::strip_thinking(blocks) } else { blocks }
}

/// Gives every tool call that has no result an error result
/// (`UNFINISHED_TOOL_CALL`), at the start of the user message that follows
/// it, or in a new user message if it was the last one. The API rejects a
/// tool call not answered in the very next message, and a turn stopped
/// by the user, or cut off by a server restart, can leave exactly that;
/// notices saved since then may follow it too. Done when building each
/// request rather than saved, since a saved result would land after those
/// notices.
#[cfg(feature = "server")]
pub(super) fn answer_unfinished_tool_calls(
    mut history: Vec<anthropic::AnthropicMessage>,
) -> Vec<anthropic::AnthropicMessage> {
    let mut i = 0;
    while i < history.len() {
        if history[i].role == "assistant" {
            let calls: Vec<String> = history[i]
                .content
                .iter()
                .filter_map(|block| match block {
                    anthropic::ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect();
            let next_is_user = history.get(i + 1).is_some_and(|m| m.role == "user");
            let answered: Vec<&String> = if next_is_user {
                history[i + 1]
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        anthropic::ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id),
                        _ => None,
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let missing: Vec<anthropic::ContentBlock> = calls
                .iter()
                .filter(|id| !answered.contains(id))
                .map(|id| anthropic::ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: UNFINISHED_TOOL_CALL.to_string(),
                    is_error: Some(true),
                })
                .collect();
            if !missing.is_empty() {
                if next_is_user {
                    history[i + 1].content.splice(0..0, missing);
                } else {
                    history.insert(
                        i + 1,
                        anthropic::AnthropicMessage {
                            role: "user".to_string(),
                            content: missing,
                        },
                    );
                }
            }
        }
        i += 1;
    }
    history
}

/// The error result a tool call gets when its turn ended before it
/// finished (the user stopped the turn, or the server restarted).
#[cfg(feature = "server")]
pub(super) const UNFINISHED_TOOL_CALL: &str = "This tool call didn't finish: the turn was stopped (by the user, or by a server restart) before it returned. Its effects, if any, are unknown.";

/// The fixed part of every turn's system prompt: who the model is, how its
/// sandbox and tools work, and how its replies are shown. Kept as prose in
/// its own file so it reads and edits like prose.
#[cfg(feature = "server")]
pub(super) const BASE_SYSTEM_PROMPT: &str = include_str!("system_prompt.md");

/// The parts of the system prompt that depend on this deployment or the
/// day, gathered per turn by `prompt_environment`. An empty list means
/// "none" (or that reading it failed, which is logged), and its line is
/// left out.
#[cfg(feature = "server")]
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PromptEnvironment {
    pub date: chrono::NaiveDate,
    pub model: String,
    /// `(name, mount path)`, the path already resolved.
    pub volumes: Vec<(String, String)>,
    pub mcp_servers: Vec<String>,
    /// The conversation's repos (SME-32).
    pub repos: Vec<crate::git::RepoSummary>,
    /// Their loaded `AGENTS.md` files.
    pub instructions: Vec<crate::git::ProjectInstructions>,
}

/// The system prompt sent with every turn: the base prompt, then an
/// environment section built from `env`. Pure, so the request and the
/// context detail view can't disagree about what's sent.
#[cfg(feature = "server")]
pub(crate) fn system_prompt(env: &PromptEnvironment) -> String {
    let mut prompt = String::from(BASE_SYSTEM_PROMPT);
    prompt.push_str("\n# Environment\n\n");
    prompt.push_str(&format!("- Today's date: {}\n", env.date.format("%Y-%m-%d")));
    prompt.push_str(&format!("- Model: {}\n", env.model));
    if !env.volumes.is_empty() {
        prompt.push_str("- Volumes (their contents survive the pod ending):\n");
        for (name, path) in &env.volumes {
            prompt.push_str(&format!("  - {name} mounted at {path}\n"));
        }
    }
    if !env.mcp_servers.is_empty() {
        prompt.push_str(&format!(
            "- MCP servers: {} (their tools are named mcp__<server>__<tool>)\n",
            env.mcp_servers.join(", ")
        ));
    }
    if !env.repos.is_empty() {
        prompt.push_str("- Repositories:\n");
        for repo in &env.repos {
            let state = match repo.status {
                crate::git::RepoStatus::Ready => repo.branch.clone().unwrap_or_default(),
                crate::git::RepoStatus::Cloning => "still cloning".to_string(),
                crate::git::RepoStatus::Failed => "clone failed".to_string(),
            };
            prompt.push_str(&format!("  - {}: {} ({state})", repo.path, repo.url));
            if !repo.agents_files.is_empty() {
                let files: Vec<String> = repo
                    .agents_files
                    .iter()
                    .map(|f| {
                        if repo.loaded_instructions.contains(f) {
                            format!("{f} (loaded)")
                        } else {
                            f.clone()
                        }
                    })
                    .collect();
                prompt.push_str(&format!("; AGENTS.md files: {}", files.join(", ")));
            }
            prompt.push('\n');
        }
    }
    prompt.push_str(&crate::git::render_project_instructions(&env.instructions));
    prompt
}

/// Gathers this turn's `PromptEnvironment`. A database read that fails
/// leaves its list empty (and is logged) rather than failing the turn: the
/// prompt without it is still useful.
#[cfg(feature = "server")]
pub(crate) async fn prompt_environment(
    pool: &PgPool,
    conversation_id: i64,
    model: &str,
) -> PromptEnvironment {
    let repos = crate::git::list_repos(pool, conversation_id).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "couldn't list repos for the system prompt");
        Vec::new()
    });
    let instructions = crate::git::project_instructions(pool, conversation_id)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "couldn't read project instructions for the system prompt");
            Vec::new()
        });
    let volumes = match db::list_sandbox_volumes(pool).await {
        Ok(volumes) => volumes.into_iter().map(|v| (v.name, v.mount_path)).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list sandbox volumes for the system prompt");
            Vec::new()
        }
    };
    let mcp_servers = match db::list_mcp_server_configs(pool).await {
        Ok(configs) => configs.into_iter().map(|c| c.name).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list MCP servers for the system prompt");
            Vec::new()
        }
    };
    PromptEnvironment {
        date: chrono::Utc::now().date_naive(),
        model: model.to_string(),
        volumes,
        mcp_servers,
        repos,
        instructions,
    }
}
