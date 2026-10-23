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
        // An empty or blank reply saved before B3's fix (SME-122): the API
        // refuses one anywhere but last, so it's left out.
        .filter_map(|mut message| {
            if message.role == "assistant" {
                message.content = without_blank_text(message.content);
                if message.content.is_empty() {
                    return None;
                }
            }
            Some(message)
        })
        .collect();
    Ok(answer_unfinished_tool_calls(history))
}

/// `blocks` without text blocks that are empty or only whitespace, which
/// the API refuses (B3, SME-122). A reply left with no blocks isn't saved.
#[cfg(feature = "server")]
pub(super) fn without_blank_text(blocks: Vec<anthropic::ContentBlock>) -> Vec<anthropic::ContentBlock> {
    blocks
        .into_iter()
        .filter(|block| !matches!(block, anthropic::ContentBlock::Text { text } if text.trim().is_empty()))
        .collect()
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
            if next_is_user {
                move_late_results(&mut history, i, &calls);
            }
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

/// Moves a result for one of `calls` (the tool calls in `history[i]`) that
/// sits in a later user message of the same run (before the next assistant
/// message) to the front of `history[i + 1]`, and drops a message the move
/// leaves empty. An `ask_user` call is answered after any notices saved
/// while it waited (SME-34); the API wants every result in the message
/// right after the call. Storage keeps the order things happened in.
#[cfg(feature = "server")]
fn move_late_results(history: &mut Vec<anthropic::AnthropicMessage>, i: usize, calls: &[String]) {
    let is_late = |block: &anthropic::ContentBlock, history: &[anthropic::AnthropicMessage]| {
        matches!(block, anthropic::ContentBlock::ToolResult { tool_use_id, .. }
            if calls.contains(tool_use_id)
                && !history[i + 1].content.iter().any(|b| matches!(b,
                    anthropic::ContentBlock::ToolResult { tool_use_id: answered, .. } if answered == tool_use_id)))
    };
    let mut moved = Vec::new();
    let mut j = i + 2;
    while j < history.len() && history[j].role == "user" {
        let (late, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut history[j].content)
            .into_iter()
            .partition(|block| is_late(block, history));
        moved.extend(late);
        if kept.is_empty() {
            history.remove(j);
        } else {
            history[j].content = kept;
            j += 1;
        }
    }
    if !moved.is_empty() {
        history[i + 1].content.splice(0..0, moved);
    }
}

/// The error result a tool call gets when its turn ended before it
/// finished (the user stopped the turn, or the server restarted).
#[cfg(feature = "server")]
pub(super) const UNFINISHED_TOOL_CALL: &str = "This tool call didn't finish: the turn was stopped (by the user, or by a server restart) before it returned. Its effects, if any, are unknown.";

/// The error result each whole tool call in a cut-off reply gets: none of
/// a cut-off reply's calls run (SME-126).
#[cfg(feature = "server")]
pub(super) const CUT_OFF_TOOL_CALL: &str = "Not run: your reply was cut off before it finished, so none of its tool calls ran. Make the call again if it's still needed.";

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
    /// Each repo's recorded persona files, by repo id (SME-235): names
    /// only, listed whether or not the repo is trusted.
    pub persona_files: std::collections::BTreeMap<i64, Vec<String>>,
    /// Set when the conversation is a child agent (SME-236).
    pub agent: Option<AgentPrompt>,
}

/// What a child agent's system prompt says about it (SME-236).
#[cfg(feature = "server")]
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AgentPrompt {
    pub id: i64,
    pub parent_id: i64,
    /// The persona it runs under, as snapshotted when it started.
    pub persona: Option<crate::db::AgentPersona>,
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
    if let Some(agent) = &env.agent {
        prompt.push_str(&format!(
            "- You are agent #{}, started by conversation #{} (your parent) with the brief that \
             starts this conversation. Call finish with your report when your work is done; \
             message_agent with to \"parent\" writes to your parent at any time. The user can \
             also write to you.\n",
            agent.id, agent.parent_id
        ));
    }
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
                // SME-272: the branch is the remote owner's choice.
                crate::git::RepoStatus::Ready => match repo.branch.as_deref() {
                    Some(b) if !crate::git::plain_path_text(b) => {
                        "a branch whose name holds control characters".to_string()
                    }
                    b => b.unwrap_or_default().to_string(),
                },
                crate::git::RepoStatus::Cloning => "still cloning".to_string(),
                crate::git::RepoStatus::Failed => "clone failed".to_string(),
            };
            prompt.push_str(&format!("  - {}: {} ({state})", repo.path, repo.url));
            prompt.push_str(&agents_files_clause(repo));
            if let Some(personas) = env.persona_files.get(&repo.id).filter(|p| !p.is_empty()) {
                prompt.push_str(&format!("; persona files: {}", personas.join(", ")));
            }
            prompt.push('\n');
        }
    }
    if let Some(persona) = env.agent.as_ref().and_then(|agent| agent.persona.as_ref()) {
        prompt.push_str(&persona_section(persona));
    }
    prompt.push_str(&crate::git::render_project_instructions(&env.instructions));
    prompt
}

/// A child's persona in its system prompt (SME-236): what it is and where
/// it came from, then its instructions, before the project's.
#[cfg(feature = "server")]
fn persona_section(persona: &crate::db::AgentPersona) -> String {
    let named = crate::agents::persona_label(&persona.persona_path).unwrap_or_else(|| persona.persona_path.clone());
    let at = match persona.persona_commit.as_deref() {
        Some(commit) => format!(" at {}", commit.get(..7).unwrap_or(commit)),
        None => String::new(),
    };
    format!(
        "\n# Persona\n\nYour persona, `{named}` from {}{at}, read when you started; a later change to \
         the file doesn't change it.\n\n{}\n",
        persona.persona_path,
        persona.persona_body.trim_end()
    )
}

/// A repo's AGENTS.md files on its Repositories line ("; AGENTS.md
/// files: …"), marking the loaded ones; empty when it has none. A path
/// recorded before SME-271 that isn't `plain_path_text` (a newline, line
/// separator or bidi control in it) is only counted, never shown.
fn agents_files_clause(repo: &crate::git::RepoSummary) -> String {
    let (shown, hidden): (Vec<&String>, Vec<&String>) =
        repo.agents_files.iter().partition(|f| crate::git::plain_path_text(f));
    let mut clause = String::new();
    if !shown.is_empty() {
        let files: Vec<String> = shown
            .into_iter()
            .map(|f| if repo.loaded_instructions.contains(f) { format!("{f} (loaded)") } else { f.clone() })
            .collect();
        clause.push_str(&format!("; AGENTS.md files: {}", files.join(", ")));
    }
    match hidden.len() {
        0 => {}
        1 => clause.push_str("; 1 AGENTS.md file not shown (a control character in its path)"),
        n => clause.push_str(&format!("; {n} AGENTS.md files not shown (control characters in their paths)")),
    }
    clause
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
    let persona_files = crate::git::persona_files_by_repo(pool, conversation_id)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "couldn't read persona files for the system prompt");
            Default::default()
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
    let agent = match agent_prompt(pool, conversation_id).await {
        Ok(agent) => agent,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read the agent's row for the system prompt");
            None
        }
    };
    PromptEnvironment {
        date: chrono::Utc::now().date_naive(),
        model: model.to_string(),
        volumes,
        mcp_servers,
        repos,
        instructions,
        persona_files,
        agent,
    }
}

/// The agent part of the prompt, `None` for a conversation without a parent.
#[cfg(feature = "server")]
async fn agent_prompt(pool: &PgPool, conversation_id: i64) -> Result<Option<AgentPrompt>, sqlx::Error> {
    let Some(parent_id) = db::conversation_parent(pool, conversation_id).await? else {
        return Ok(None);
    };
    let persona = db::agent_persona(pool, conversation_id).await?;
    Ok(Some(AgentPrompt { id: conversation_id, parent_id, persona }))
}
