use dioxus::fullstack::ServerEvents;
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

use crate::models::{Conversation, Message};
use crate::{anthropic, events};

#[cfg(feature = "server")]
use crate::db;
#[cfg(feature = "server")]
use crate::sandbox;

#[cfg(feature = "server")]
use sqlx::PgPool;

#[cfg(feature = "server")]
use std::collections::HashMap;
#[cfg(feature = "server")]
use std::sync::{Arc, LazyLock, Mutex};

#[get("/api/conversations")]
pub async fn get_conversations() -> ServerFnResult<Vec<Conversation>> {
    db::list_conversations(db::get())
        .await
        .map_err(ServerFnError::new)
}

#[post("/api/conversations")]
pub async fn create_conversation() -> ServerFnResult<Conversation> {
    db::create_conversation(db::get())
        .await
        .map_err(ServerFnError::new)
}

#[get("/api/conversations/{id}/messages")]
pub async fn get_messages(id: i64) -> ServerFnResult<Vec<Message>> {
    if !db::conversation_exists(db::get(), id)
        .await
        .map_err(ServerFnError::new)?
    {
        return Err(ServerFnError::new("conversation not found"));
    }
    db::list_messages(db::get(), id)
        .await
        .map_err(ServerFnError::new)
}

#[delete("/api/conversations/{id}")]
pub async fn delete_conversation(id: i64) -> ServerFnResult<()> {
    // Best-effort, unconditional (unlike terminate_pod, which the model
    // calls and which is guarded) — the conversation is going away
    // regardless, so nothing about the pod matters anymore either way.
    crate::sandbox::teardown_conversation(db::get(), id).await;
    let _ = crate::browsing::close_session(id).await;
    anthropic::tools::forget_conversation_tasks(id);
    db::delete_conversation(db::get(), id)
        .await
        .map_err(ServerFnError::new)?;
    crate::events::forget(id);
    forget_conversation_lock(id);
    // After the delete, so a listener refetching sees the pod rows gone.
    crate::events::publish_app(crate::events::AppEvent::PodsChanged);
    Ok(())
}

#[cfg(feature = "server")]
fn anthropic_model() -> String {
    std::env::var("ANTHROPIC_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "claude-opus-4-8".to_string())
}

/// On by default — set `ANTHROPIC_THINKING=0` (or `false`/`off`) to turn it
/// back off. Was briefly made opt-in after thinking broke tool use against
/// a local Ollama `gpt-oss` model (Ollama's Anthropic-compatibility shim
/// doesn't cleanly split the model's reasoning out of a tool call's
/// arguments the way real Anthropic does — see
/// `is_ollama_thinking_tool_call_corruption`) — safe to default back on
/// now that `run_turn_bounded` retries that *specific* failure without
/// thinking instead of surfacing a raw 500, rather than requiring everyone
/// to manually opt in just to get thinking against the real API.
#[cfg(feature = "server")]
fn thinking_enabled() -> bool {
    !matches!(
        std::env::var("ANTHROPIC_THINKING").as_deref(),
        Ok("0" | "false" | "off")
    )
}

/// Ollama's Anthropic-compatibility shim can fail to turn a model's raw
/// output into a valid tool call, surfacing as a flat 500 with a message
/// like `error parsing tool call: raw='...', err=...` instead of streaming
/// normally. Two different root causes share this exact shape (at least
/// for `gpt-oss`-family models): thinking's reasoning landing in the same
/// text the shim expected pure tool-call JSON in (see `thinking_enabled`'s
/// doc comment), or the model just writing invalid JSON on its own — e.g.
/// a bare `?` for a value it wasn't sure how to fill in. Real Anthropic
/// never returns this. Deliberately narrow (a distinctive, Ollama-specific
/// phrase, not just "any 500") so an unrelated upstream failure doesn't
/// silently double latency/cost by retrying pointlessly.
#[cfg(feature = "server")]
fn is_ollama_thinking_tool_call_corruption(message: &str) -> bool {
    message.contains("error parsing tool call")
}

/// Extra attempts (beyond the first) for the failure
/// `is_ollama_thinking_tool_call_corruption` recognizes — the first retry
/// also drops `thinking` (see the call site), remaining retries are plain
/// regenerations, since a local model's next sampling pass often just
/// doesn't repeat the same malformed JSON. Bounded so a call the model is
/// reliably bad at can't retry forever.
#[cfg(feature = "server")]
const TOOL_CALL_PARSE_RETRIES: usize = 2;

/// Bound on how many tool-use turns one `run_turn` call will chase before
/// giving up. Raised from the original placeholder of 5 (fine for `add`/
/// `count`, hit almost immediately by a real multi-step coding session using
/// the sandbox terminal tools) — still not a load-bearing safety limit, just
/// a backstop against looping forever. Exceeding it ends the turn with an
/// error rather than looping forever.
#[cfg(feature = "server")]
const MAX_TURNS: usize = 10_000;

/// Every real turn's requested reply budget — shared with the
/// auto-compaction trigger below, which reserves at least this much
/// headroom off the context window before deciding a request is too big.
#[cfg(feature = "server")]
const MAX_TOKENS: u32 = 16_384;

/// Extra headroom reserved on top of `MAX_TOKENS`, in case a smaller
/// `max_tokens` is ever configured per-request in the future — mirrors
/// opencode's own `max(output reserve, buffer)` term. Not currently
/// reachable as its own binding factor since `MAX_TOKENS` already exceeds
/// it, but kept as a named floor rather than assuming `MAX_TOKENS` always
/// will. See docs/projects/plans/auto-compaction.md.
#[cfg(feature = "server")]
const COMPACTION_SAFETY_BUFFER: u32 = 4096;

/// A cheap, approximate token-count estimate for content not yet sent —
/// chars/4, Anthropic's own rough published guidance, not a real
/// tokenizer. Used only to decide whether compaction should run before a
/// request goes out; never persisted or shown as if it were exact usage.
///
/// Spiked once against the real (non-mock) gateway configured in dev,
/// rather than left as a pure assumption: two consecutive real turns'
/// `usage.input_tokens` differ by (previous turn's real `output_tokens` +
/// whatever new message content was added) — isolating that second term
/// for an 800-character message gave ~211 real tokens against this
/// function's 200-token estimate, ~3.8 real chars/token vs. the 4.0
/// assumed here. Close enough not to revisit without a reason to — and
/// that one data point (repeated single characters) is a harder case for
/// a tokenizer than real prose, which should compress to *more*
/// chars/token, not fewer, so if anything this errs slightly generous
/// already. Same method (diff two real `get_context_usage` reads, subtract
/// the earlier one's `output_tokens`) works to re-check this later against
/// a real, varied conversation if the constant is ever in question.
#[cfg(feature = "server")]
fn estimate_tokens(blocks: &[anthropic::ContentBlock]) -> u64 {
    let chars: usize = blocks
        .iter()
        .map(|block| match block {
            anthropic::ContentBlock::Text { text } => text.len(),
            anthropic::ContentBlock::ToolResult { content, .. } => content.len(),
            anthropic::ContentBlock::ToolUse { input, .. } => input.to_string().len(),
            anthropic::ContentBlock::Thinking {
                thinking,
                signature,
            } => thinking.len() + signature.len(),
            anthropic::ContentBlock::CompactionSummary { summary, .. } => summary.len(),
            anthropic::ContentBlock::CompactionPlaceholder { text } => text.len(),
        })
        .sum();
    (chars / 4) as u64
}

/// Whether the upcoming request's real size looks likely to eat into the
/// reserved ceiling (model output allowance + a safety buffer, subtracted
/// from the context window) — opencode's own trigger mechanism, checked
/// *before* a request is sent rather than reacting to a real rejection.
/// `last_usage` is the last real response's usage (the ground truth for
/// everything already in the conversation — see `stream::StreamedTurn`'s
/// own doc comment); `new_content_estimate` is `estimate_tokens` applied
/// to whatever's freshly being added this turn. `None` `last_usage`
/// (nothing sent yet) never triggers — there's nothing to compact. See
/// docs/projects/plans/auto-compaction.md.
#[cfg(feature = "server")]
fn should_compact(
    last_usage: Option<&anthropic::TokenUsage>,
    new_content_estimate: u64,
    context_window: u32,
) -> bool {
    let Some(last_usage) = last_usage else {
        return false;
    };
    let already_used = (last_usage.input_tokens
        + last_usage.output_tokens
        + last_usage.cache_creation_input_tokens
        + last_usage.cache_read_input_tokens)
        .max(0) as u64;
    let projected = already_used + new_content_estimate;
    let reserved = MAX_TOKENS.max(COMPACTION_SAFETY_BUFFER) as u64;
    let ceiling = (context_window as u64).saturating_sub(reserved);
    projected >= ceiling
}

/// Whether it's safe to end a compaction's covered range right after a
/// message with this content — i.e. it contains no `tool_use` block whose
/// paired `tool_result` (always the very next persisted message, in this
/// codebase's per-turn persistence model) hasn't been included too.
/// Anthropic rejects a request with an orphaned half outright, so this is
/// a hard constraint on where `covers_through_message_id` may point, never
/// best-effort. See docs/projects/plans/auto-compaction.md.
#[cfg(feature = "server")]
fn is_safe_compaction_boundary(blocks: &[anthropic::ContentBlock]) -> bool {
    !blocks
        .iter()
        .any(|block| matches!(block, anthropic::ContentBlock::ToolUse { .. }))
}

/// A fixed, structural placeholder — never shown as if the user actually
/// typed it, just what makes the synthetic post-compaction exchange below
/// start validly with `user` (Anthropic's own requirement, not negotiable).
#[cfg(feature = "server")]
const COMPACTION_PLACEHOLDER_PROMPT: &str = "Summarize the conversation so far.";

/// The synthetic message that actually gets a real response after
/// compaction — the summary already carries forward whatever the model
/// needs (including the gist of whatever large content triggered
/// compaction in the first place, since the summarization call sees
/// everything up through it), so this is a plain continuation nudge, not
/// a replay of anything.
#[cfg(feature = "server")]
const COMPACTION_CONTINUATION_PROMPT: &str = "Continue based on the summary above.";

/// The three synthetic messages one compaction pass inserts, in this
/// order — pure construction, no I/O (see `compact_conversation` for what
/// actually persists them). Three messages, not one, because Anthropic
/// requires strict user/assistant alternation *and* that `messages` start
/// with `user` — a single summary message can't satisfy both "starts with
/// user" and "ends with something real to respond to" on its own:
/// `user` (structural placeholder) -> `assistant` (the summary) -> `user`
/// (a plain continuation nudge, the thing the *next* real request
/// actually responds to). See docs/projects/plans/auto-compaction.md.
#[cfg(feature = "server")]
fn compaction_messages(
    summary: String,
    covers_through_message_id: i64,
) -> [(&'static str, Vec<anthropic::ContentBlock>); 3] {
    [
        (
            "user",
            vec![anthropic::ContentBlock::CompactionPlaceholder {
                text: COMPACTION_PLACEHOLDER_PROMPT.to_string(),
            }],
        ),
        (
            "assistant",
            vec![anthropic::ContentBlock::CompactionSummary {
                summary,
                covers_through_message_id,
            }],
        ),
        (
            "user",
            vec![anthropic::ContentBlock::CompactionPlaceholder {
                text: COMPACTION_CONTINUATION_PROMPT.to_string(),
            }],
        ),
    ]
}

/// Builds the message history actually replayed to Anthropic — every
/// persisted message, *except* the compaction boundary, if any, changes
/// what's replayed without anything having been rewritten or deleted in
/// storage. Finds the *latest* `CompactionSummary` block across every
/// message (a long conversation can compact more than once; only the most
/// recent boundary matters — everything at or before it, including any
/// earlier compaction's own summary message, is superseded), skips every
/// message at or before that boundary, and translates the summary itself
/// from `CompactionSummary` into a plain `Text` block — Anthropic has no
/// concept of the former. See docs/projects/plans/auto-compaction.md.
#[cfg(feature = "server")]
fn history_for_request(
    messages: Vec<Message>,
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
        .map(|(_, role, blocks)| {
            let content = blocks
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

/// Gives every tool call that has no result an error result
/// (`UNFINISHED_TOOL_CALL`), at the start of the user message that follows
/// it, or in a new user message if it was the last one. The API rejects a
/// tool call not answered in the very next message, and a turn stopped
/// by the user, or cut off by a server restart, can leave exactly that;
/// notices saved since then may follow it too. Done when building each
/// request rather than saved, since a saved result would land after those
/// notices.
#[cfg(feature = "server")]
fn answer_unfinished_tool_calls(
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
const UNFINISHED_TOOL_CALL: &str = "This tool call didn't finish: the turn was stopped (by the user, or by a server restart) before it returned. Its effects, if any, are unknown.";

/// The fixed part of every turn's system prompt: who the model is, how its
/// sandbox and tools work, and how its replies are shown. Kept as prose in
/// its own file so it reads and edits like prose.
#[cfg(feature = "server")]
const BASE_SYSTEM_PROMPT: &str = include_str!("system_prompt.md");

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
    prompt
}

/// Gathers this turn's `PromptEnvironment`. A database read that fails
/// leaves its list empty (and is logged) rather than failing the turn: the
/// prompt without it is still useful.
#[cfg(feature = "server")]
pub(crate) async fn prompt_environment(pool: &PgPool) -> PromptEnvironment {
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
        model: anthropic_model(),
        volumes,
        mcp_servers,
    }
}

/// The system prompt for the dedicated summarization call `compact_conversation`
/// makes — no tools attached, so this is the model's only instruction.
/// Explicitly demands every live id be preserved verbatim rather than
/// trusting the model to notice one buried in a long transcript
/// unprompted — see `describe_live_state` below, which hands them over
/// directly rather than leaving them to be spotted.
#[cfg(feature = "server")]
const COMPACTION_SYSTEM_PROMPT: &str = "You are compacting an AI coding agent's \
conversation history to free up context window space. Write a concise summary \
of the conversation so far: the user's original goal, key decisions made, the \
current state of any files or code changed, and any unresolved next steps. You \
MUST explicitly mention, by its exact id, every sandbox pod, terminal, and \
background task listed below as currently live — never omit or paraphrase an \
id, since later tool calls still need a working reference to it. Write the \
summary as plain prose.";

/// A plain-text listing of every currently-live pod/terminal/task for
/// `conversation_id`, handed to the summarization call so it can be told
/// directly what must survive — see `COMPACTION_SYSTEM_PROMPT` and
/// docs/projects/plans/auto-compaction.md's "Interaction with live/
/// in-flight state." Best-effort: a lookup failure just omits that
/// category rather than failing the whole compaction over it.
#[cfg(feature = "server")]
async fn describe_live_state(pool: &PgPool, conversation_id: i64) -> String {
    // Deliberately the plain `db::` row queries, not `sandbox::list_pods`/
    // `list_terminals` — those also reach through the process-global
    // sandbox manager to check live connection status, which panics if
    // `sandbox::init()` was never called (true of most tests, and not
    // otherwise relevant here: a summarization prompt just needs which
    // ids exist and aren't terminated, not real-time connection health).
    let mut lines = Vec::new();
    if let Ok(pods) = db::list_sandbox_pods(pool, conversation_id).await {
        for pod in pods {
            lines.push(format!("- sandbox pod_id {}", pod.id));
        }
    }
    if let Ok(terminals) = db::list_sandbox_terminals_for_conversation(pool, conversation_id).await
    {
        for terminal in terminals {
            lines.push(format!(
                "- terminal_id {} in pod_id {}",
                terminal.id, terminal.pod_id
            ));
        }
    }
    for task in anthropic::tools::snapshot_tasks(conversation_id) {
        lines.push(format!(
            "- background task_id {} ({}, status: {})",
            task.task_id, task.tool, task.status
        ));
    }
    if let Ok(todos) = db::get_conversation_todos(pool, conversation_id).await {
        for todo in todos {
            lines.push(format!(
                "- todo: {} (status: {})",
                todo.content,
                todo.status.as_str()
            ));
        }
    }
    if lines.is_empty() {
        "(nothing currently live)".to_string()
    } else {
        lines.join("\n")
    }
}

/// The conversation as plain text, for the summarization call: the latest
/// compaction's summary and everything since (what came before it is
/// already in that summary — including it again made every later
/// compaction bigger than the last, until the summarization call couldn't
/// fit at all), cut to `max_chars` by dropping the oldest part.
#[cfg(feature = "server")]
fn compaction_transcript(messages: &[Message], max_chars: usize) -> String {
    let latest_boundary = messages
        .iter()
        .filter_map(|m| m.blocks().ok())
        .flatten()
        .filter_map(|block| match block {
            anthropic::ContentBlock::CompactionSummary {
                covers_through_message_id,
                ..
            } => Some(covers_through_message_id),
            _ => None,
        })
        .max();
    let mut transcript = String::new();
    for message in messages
        .iter()
        .filter(|m| latest_boundary.is_none_or(|boundary| m.id > boundary))
    {
        let Ok(blocks) = message.blocks() else {
            continue;
        };
        for block in blocks {
            let text = match block {
                anthropic::ContentBlock::Text { text } => text,
                anthropic::ContentBlock::ToolUse { name, input, .. } => {
                    format!("[called tool {name} with {input}]")
                }
                anthropic::ContentBlock::ToolResult { content, .. } => {
                    format!("[tool result: {content}]")
                }
                anthropic::ContentBlock::Thinking { .. } => continue,
                anthropic::ContentBlock::CompactionSummary { summary, .. } => summary,
                // Purely structural (see its own doc comment) — noise for
                // a *later* compaction's own summarization transcript, not
                // real prior dialogue worth feeding back in.
                anthropic::ContentBlock::CompactionPlaceholder { .. } => continue,
            };
            transcript.push_str(&message.role);
            transcript.push_str(": ");
            transcript.push_str(&text);
            transcript.push('\n');
        }
    }
    let total = transcript.chars().count();
    if total <= max_chars {
        return transcript;
    }
    const OMITTED: &str = "[earlier conversation omitted]\n";
    let keep = max_chars.saturating_sub(OMITTED.chars().count());
    let tail: String = transcript.chars().skip(total - keep).collect();
    format!("{OMITTED}{tail}")
}

/// How much transcript the summarization call can take: the context window
/// less room for its instructions, the live-state listing and its own
/// output, at a conservative 3 characters per token.
#[cfg(feature = "server")]
fn compaction_transcript_budget() -> usize {
    (context_window() as usize).saturating_sub(8_192) * 3
}

/// Runs one compaction pass: summarizes everything currently persisted (up
/// through and including whatever's pending — the size trigger fired
/// because of everything currently in the conversation, not just the
/// older part of it) via a separate, tools-less Anthropic call, and
/// inserts `compaction_messages`' three synthetic messages. Nothing
/// already stored is rewritten or removed. A no-op (`Ok(())`, no API call)
/// if there's nothing to compact, or if the current last message isn't a
/// safe boundary (shouldn't happen given the loop's own invariant — see
/// `is_safe_compaction_boundary` — but checked defensively rather than
/// assumed). If the summarization call itself fails, this propagates the
/// error and the whole turn fails loudly, rather than proceeding with the
/// oversized request compaction exists to prevent — see the plan's
/// "Resolved" decision on this.
#[cfg(feature = "server")]
async fn compact_conversation(
    pool: &PgPool,
    conversation_id: i64,
    api_key: Option<&str>,
    auth_token: Option<&str>,
) -> ServerFnResult<()> {
    let messages = db::list_messages(pool, conversation_id)
        .await
        .map_err(ServerFnError::new)?;
    let Some(last) = messages.last() else {
        return Ok(());
    };
    let last_blocks = last.blocks().map_err(ServerFnError::new)?;
    if !is_safe_compaction_boundary(&last_blocks) {
        return Ok(());
    }
    let covers_through_message_id = last.id;

    let transcript = compaction_transcript(&messages, compaction_transcript_budget());

    let live_state = describe_live_state(pool, conversation_id).await;
    let prompt = format!(
        "Conversation so far:\n{transcript}\n\nCurrently live (preserve these \
         exact ids in your summary):\n{live_state}"
    );

    let summarization_request = anthropic::CreateMessageRequest {
        model: anthropic_model(),
        max_tokens: 2048,
        system: Some(COMPACTION_SYSTEM_PROMPT.to_string()),
        messages: vec![anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text { text: prompt }],
        }],
        stream: true,
        tools: vec![],
        thinking: None,
    };

    let mut discard_deltas = |_: &str| {};
    let turn = anthropic::stream::stream_anthropic_message(
        api_key,
        auth_token,
        &summarization_request,
        &mut discard_deltas,
    )
    .await
    .map_err(ServerFnError::new)?;
    let summary = turn
        .content
        .into_iter()
        .find_map(|block| match block {
            anthropic::ContentBlock::Text { text } => Some(text),
            _ => None,
        })
        .unwrap_or_default();

    for (role, content) in compaction_messages(summary, covers_through_message_id) {
        db::create_message(pool, conversation_id, role, &content)
            .await
            .map_err(ServerFnError::new)?;
    }
    Ok(())
}

/// Saves `text` as a `user` notice in `conversation_id` once no turn is
/// running there, by taking the conversation's turn lock first — so the
/// notice can never land between a tool call and its result, an order the
/// API rejects. For things that happen outside a turn and that the model
/// should learn about: its pod crashing, or the user stopping it. Waits
/// for a running turn to end, so call it from a spawned task.
#[cfg(feature = "server")]
pub(crate) async fn save_notice_between_turns(
    pool: &PgPool,
    conversation_id: i64,
    text: String,
) -> Result<Message, sqlx::Error> {
    let lock = conversation_lock(conversation_id);
    let _turn = lock.lock().await;
    let saved = db::create_message(
        pool,
        conversation_id,
        "user",
        &[anthropic::ContentBlock::Text { text }],
    )
    .await?;
    crate::events::publish(
        conversation_id,
        crate::events::ConversationEvent::MessagesAppended {
            messages: vec![saved.clone()],
        },
    );
    Ok(saved)
}

/// A live `send_message` call and a background task's push-triggered
/// `run_turn` call (or two different tasks' pushes) can race for the same
/// conversation — Anthropic's strict user/assistant alternation breaks if
/// two writers persist a turn at once. Keyed by conversation id; which
/// caller acquires a given conversation's lock first when several are ready
/// is unspecified (see the plan's Open questions).
#[cfg(feature = "server")]
static CONVERSATION_LOCKS: LazyLock<Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(feature = "server")]
fn conversation_lock(conversation_id: i64) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = CONVERSATION_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    locks
        .entry(conversation_id)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Drops `conversation_id`'s lock, for when the conversation is deleted.
/// A turn still holding it keeps its own handle; any later turn gets a
/// fresh lock and then fails, since the conversation no longer exists.
#[cfg(feature = "server")]
fn forget_conversation_lock(conversation_id: i64) {
    CONVERSATION_LOCKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&conversation_id);
    TURN_STOPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&conversation_id);
    PAUSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&conversation_id);
}

/// The credential-requirement decision `run_turn_bounded` makes at the top
/// of every call, pulled out as a pure function over already-read env
/// values — testable without any env-var mutation, locking, or thread
/// coordination at all (mutating real `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN`
/// process env vars from a test is fundamentally unsound across concurrent
/// test threads, `getenv`/`setenv` not being thread-safe at the OS level —
/// this sidesteps that entirely). At least one of `api_key`/`auth_token`
/// must be present (either is enough — `ANTHROPIC_AUTH_TOKEN` is what a
/// Hugging Face-hosted Anthropic-compatible endpoint uses instead of a
/// real Anthropic API key).
#[cfg(feature = "server")]
fn require_at_least_one_credential(
    api_key: &Option<String>,
    auth_token: &Option<String>,
) -> Result<(), String> {
    if api_key.is_none() && auth_token.is_none() {
        Err("neither ANTHROPIC_API_KEY nor ANTHROPIC_AUTH_TOKEN is set on the server".to_string())
    } else {
        Ok(())
    }
}

/// Runs one full tool-use round trip for `conversation_id`: persists
/// `new_message`, then loops calling the real Anthropic API — executing any
/// tool the model asks for and persisting its result — until the model
/// produces a non-`tool_use` turn or `MAX_TURNS` is exceeded. Returns every
/// message persisted along the way, in order, starting with `new_message`
/// itself. `send_message` wires a live `on_delta` into the browser's SSE
/// stream for the token-by-token typing effect; a later stage's
/// background-task push notification calls this with `on_delta = None`.
/// Returns a boxed, type-erased future rather than using plain `async fn`
/// sugar: `run_turn` and `anthropic::tools::execute` call each other
/// (`execute`'s `run_async` branch spawns a task that can call back into
/// `run_turn` to push a notification, which calls `execute` again for the
/// *next* turn's tool calls) — that mutual recursion defeats rustc's
/// `Send`-auto-trait inference for plain `async fn`s ("cannot satisfy `impl
/// Future: Send`" with no useful location). Type-erasing one edge of the
/// cycle here breaks it.
#[cfg(feature = "server")]
pub(crate) fn run_turn<'a>(
    pool: &'a PgPool,
    conversation_id: i64,
    new_message: anthropic::AnthropicMessage,
    on_delta: Option<&'a mut (dyn FnMut(&str) + Send)>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ServerFnResult<Vec<Message>>> + Send + 'a>>
{
    run_turn_bounded(
        pool,
        conversation_id,
        Some(new_message),
        on_delta,
        MAX_TURNS,
    )
}

/// Wakes `conversation_id`'s turn loop because a terminal command reached a
/// terminal state (finished or lost) — no synthetic message of its own,
/// unlike `run_turn`; just triggers the same backlog drain
/// `run_turn_bounded`'s loop already does on every iteration. A true no-op
/// if, by the time this acquires the conversation lock, nothing is actually
/// pending (e.g. another concurrent wake, or an unrelated live message,
/// already handled it) — no persisted message, no API call. This is what
/// keeps several commands finishing close together from costing one model
/// turn each. See `docs/projects/plans/terminal-exit-notify.md`.
///
/// On failure, publishes `ConversationEvent::NotificationDeliveryFailed`
/// (alongside a `tracing::warn!`) so a watching browser tab sees it live —
/// there's usually no `send_message` call in flight to relay a `ChatEvent`
/// through, since the whole point of this function is firing with no
/// request active. The underlying notification text is unaffected either
/// way: it's already durably persisted by the drain step, which runs and
/// commits *before* the API call that might fail, so this only means the
/// model hasn't been prompted with it yet, not that it's lost.
#[cfg(feature = "server")]
pub(crate) async fn wake_conversation(
    pool: &PgPool,
    conversation_id: i64,
) -> ServerFnResult<Vec<Message>> {
    // After the user stopped this conversation, pending notices wait for
    // their next message, which drains them the same way (see `PAUSED`).
    if is_paused(conversation_id) {
        return Ok(Vec::new());
    }
    let result = run_turn_bounded(pool, conversation_id, None, None, MAX_TURNS).await;
    // A stop is the user's doing, not a failure to reach the model.
    if let Err(e) = &result
        && chat_error_text(e) != TURN_STOPPED
    {
        tracing::warn!(conversation_id, error = %e, "wake_conversation failed to notify the model");
        crate::events::publish(
            conversation_id,
            crate::events::ConversationEvent::NotificationDeliveryFailed {
                detail: chat_error_text(e),
            },
        );
    }
    result
}

/// The real body of `run_turn`, with the turn-loop bound as a parameter —
/// exists only so `test_run_turn_errors_when_max_turns_exceeded` can prove
/// the "give up and error" behavior without actually replaying the mock
/// upstream `MAX_TURNS` (10,000) times. Every other caller goes through
/// `run_turn`, which always passes the real `MAX_TURNS`.
/// Drains any terminal commands finished (or lost) but not yet notified
/// into `persisted` as ordinary persisted `user` messages, folding their
/// content into `pending_new_content` too (the compaction trigger's
/// "what's new since the last real response" estimate — see
/// `run_turn_bounded`'s own use of it) — pulled out of `run_turn_bounded`'s
/// loop so `wake_conversation` (which has no synthetic message of its own
/// to send) can trigger exactly this step directly, without duplicating
/// the notification-text logic.
#[cfg(feature = "server")]
async fn drain_unnotified_terminal_commands(
    pool: &PgPool,
    conversation_id: i64,
    pending_new_content: &mut Vec<anthropic::ContentBlock>,
    persisted: &mut Vec<Message>,
) -> ServerFnResult<()> {
    for command in db::unnotified_finished_terminal_commands(pool, conversation_id)
        .await
        .map_err(ServerFnError::new)?
    {
        let text = if command.status == "finished" {
            format!(
                "Terminal command {} finished: exit code {}.",
                command.command_id,
                command
                    .exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            )
        } else {
            format!(
                "Terminal command {}'s outcome is unknown — the terminal became \
                 unreachable while it was running.",
                command.command_id
            )
        };
        let notification_content = vec![anthropic::ContentBlock::Text { text }];
        let saved = db::create_message(pool, conversation_id, "user", &notification_content)
            .await
            .map_err(ServerFnError::new)?;
        pending_new_content.extend(notification_content);
        record_saved(conversation_id, persisted, saved);
        db::mark_terminal_command_notified(pool, &command.command_id)
            .await
            .map_err(ServerFnError::new)?;
    }
    Ok(())
}

/// The reply text streamed so far in each conversation's current model
/// call, for a tab that connects mid-reply (`get_reply_in_progress`).
/// Cleared when a call starts, when its reply is saved, and when the turn
/// ends.
#[cfg(feature = "server")]
static REPLIES_IN_PROGRESS: LazyLock<Mutex<HashMap<i64, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `conversation_id`'s reply so far, if a model call is streaming one.
#[cfg(feature = "server")]
pub(crate) fn reply_in_progress(conversation_id: i64) -> Option<String> {
    REPLIES_IN_PROGRESS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&conversation_id)
        .filter(|text| !text.is_empty())
        .cloned()
}

/// Ends `conversation_id`'s reply in progress (see `REPLIES_IN_PROGRESS`).
#[cfg(feature = "server")]
fn clear_reply_in_progress(conversation_id: i64) {
    REPLIES_IN_PROGRESS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&conversation_id);
}

/// Adds a message the turn just saved to `persisted` and tells every tab
/// watching the conversation, right away: the user's own message first,
/// then each step of a multi-step reply as it happens, rather than one
/// batch when the turn ends.
#[cfg(feature = "server")]
fn record_saved(conversation_id: i64, persisted: &mut Vec<Message>, saved: Message) {
    crate::events::publish(
        conversation_id,
        crate::events::ConversationEvent::MessagesAppended {
            messages: vec![saved.clone()],
        },
    );
    persisted.push(saved);
}

/// Per conversation, a counter bumped each time the user stops its turn.
/// A turn notes the value when it starts (before waiting for the turn
/// lock) and ends as soon as it changes, so a stop ends the running turn
/// and any queued behind it, but not turns that start afterwards.
#[cfg(feature = "server")]
static TURN_STOPS: LazyLock<Mutex<HashMap<i64, tokio::sync::watch::Sender<u64>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A receiver for `conversation_id`'s stop counter, with its current value
/// already seen, so only a later stop wakes it.
#[cfg(feature = "server")]
fn stop_receiver(conversation_id: i64) -> tokio::sync::watch::Receiver<u64> {
    let mut stops = TURN_STOPS.lock().unwrap_or_else(|e| e.into_inner());
    let mut receiver = stops
        .entry(conversation_id)
        .or_insert_with(|| tokio::sync::watch::channel(0).0)
        .subscribe();
    receiver.borrow_and_update();
    receiver
}

/// Stops `conversation_id`'s running turn, and any queued behind it. The
/// pod, its terminals and running commands are left alone. A no-op when
/// nothing is running.
#[cfg(feature = "server")]
pub(crate) fn stop_turn_now(conversation_id: i64) {
    // Paused even when nothing is running: the user asked the model to
    // stop, so a command finishing right after shouldn't start it again.
    PAUSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(conversation_id);
    let stops = TURN_STOPS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(stop) = stops.get(&conversation_id) {
        stop.send_modify(|count| *count += 1);
    }
}

/// Conversations the user has stopped and not written in since. While
/// paused, a finished command or background task doesn't wake the model:
/// its notice is still saved, and the model sees it on the user's next
/// message. Otherwise a stop would be undone seconds later by whatever was
/// still running. In memory: a restart un-pauses, which is harmless.
#[cfg(feature = "server")]
static PAUSED: LazyLock<Mutex<std::collections::HashSet<i64>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

/// Whether `conversation_id` is paused after a stop (see `PAUSED`).
#[cfg(feature = "server")]
pub(crate) fn is_paused(conversation_id: i64) -> bool {
    PAUSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&conversation_id)
}

/// Ends the pause after a stop: the user has written again.
#[cfg(feature = "server")]
fn resume_turns(conversation_id: i64) {
    PAUSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&conversation_id);
}

/// How many turns are running or queued, per conversation, for the Stop
/// button (`TurnState`).
#[cfg(feature = "server")]
static TURNS_IN_FLIGHT: LazyLock<Mutex<HashMap<i64, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Counts a turn as in flight for as long as it lives, including when it's
/// dropped part-way by a stop, and publishes `TurnState` when the
/// conversation goes from idle to busy or back.
#[cfg(feature = "server")]
struct TurnInFlight(i64);

#[cfg(feature = "server")]
impl TurnInFlight {
    fn start(conversation_id: i64) -> Self {
        let first = {
            let mut counts = TURNS_IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
            let count = counts.entry(conversation_id).or_insert(0);
            *count += 1;
            *count == 1
        };
        if first {
            crate::events::publish(
                conversation_id,
                crate::events::ConversationEvent::TurnState { running: true },
            );
        }
        TurnInFlight(conversation_id)
    }
}

#[cfg(feature = "server")]
impl Drop for TurnInFlight {
    fn drop(&mut self) {
        let last = {
            let mut counts = TURNS_IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
            match counts.get_mut(&self.0) {
                Some(count) if *count > 1 => {
                    *count -= 1;
                    false
                }
                _ => {
                    counts.remove(&self.0);
                    true
                }
            }
        };
        if last {
            // A stopped or failed turn leaves no reply in progress.
            clear_reply_in_progress(self.0);
            crate::events::publish(
                self.0,
                crate::events::ConversationEvent::TurnState { running: false },
            );
        }
    }
}

/// Whether `conversation_id` has a turn running or queued.
#[cfg(feature = "server")]
pub(crate) fn turn_running(conversation_id: i64) -> bool {
    TURNS_IN_FLIGHT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&conversation_id)
}

/// A turn, ended early if the user stops it (see `TURN_STOPS`). Stopping
/// drops `run_turn_body` wherever it's waiting (the model's stream, a
/// tool call, a compaction), which releases the turn lock. What it leaves
/// behind: a streaming reply isn't saved, and tool calls without results
/// get answered on the next request (`answer_unfinished_tool_calls`).
#[cfg(feature = "server")]
fn run_turn_bounded<'a>(
    pool: &'a PgPool,
    conversation_id: i64,
    new_message: Option<anthropic::AnthropicMessage>,
    on_delta: Option<&'a mut (dyn FnMut(&str) + Send)>,
    max_turns: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ServerFnResult<Vec<Message>>> + Send + 'a>>
{
    Box::pin(async move {
        let mut stop = stop_receiver(conversation_id);
        let _in_flight = TurnInFlight::start(conversation_id);
        tokio::select! {
            result = run_turn_body(pool, conversation_id, new_message, on_delta, max_turns) => result,
            _ = stop.changed() => Err(ServerFnError::new(TURN_STOPPED)),
        }
    })
}

/// The error a stopped turn ends with. Not server-only: the chat page
/// recognizes it to show "Stopped." instead of an error.
pub const TURN_STOPPED: &str = "stopped by the user";

/// What `send_message` does: checks the conversation exists, then runs
/// the user's turn in the background and returns without waiting for it.
/// The turn's messages, reply text and end reach every tab on the
/// conversation's event stream (`MessagesAppended`, `ReplyDelta`,
/// `TurnState`), and a failure or stop as `TurnError`. So a sending tab
/// holds no connection of its own while the reply streams.
#[cfg(feature = "server")]
pub(crate) async fn start_turn(pool: PgPool, id: i64, content: String) -> ServerFnResult<()> {
    if !db::conversation_exists(&pool, id)
        .await
        .map_err(ServerFnError::new)?
    {
        return Err(ServerFnError::new("conversation not found"));
    }
    // The user writing again ends any pause from an earlier stop.
    resume_turns(id);
    let new_message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text { text: content }],
    };
    tokio::spawn(async move {
        if let Err(e) = run_turn(&pool, id, new_message, None).await {
            crate::events::publish(
                id,
                crate::events::ConversationEvent::TurnError {
                    message: chat_error_text(&e),
                },
            );
        }
    });
    Ok(())
}

/// The user's Stop button: ends this conversation's running turn, and
/// keeps finished commands and tasks from starting another until they
/// write again. See `stop_turn_now`.
#[post("/api/conversations/{id}/stop")]
pub async fn stop_turn(id: i64) -> ServerFnResult<()> {
    stop_turn_now(id);
    Ok(())
}

/// Whether a turn is running in this conversation: the Stop button's
/// snapshot on (re)connect, kept current by `ConversationEvent::TurnState`.
#[get("/api/conversations/{id}/turn")]
pub async fn get_turn_state(id: i64) -> ServerFnResult<bool> {
    Ok(turn_running(id))
}

#[cfg(feature = "server")]
fn run_turn_body<'a>(
    pool: &'a PgPool,
    conversation_id: i64,
    new_message: Option<anthropic::AnthropicMessage>,
    mut on_delta: Option<&'a mut (dyn FnMut(&str) + Send)>,
    max_turns: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ServerFnResult<Vec<Message>>> + Send + 'a>>
{
    Box::pin(async move {
        let lock = conversation_lock(conversation_id);
        let _guard = lock.lock().await;

        if !db::conversation_exists(pool, conversation_id)
            .await
            .map_err(ServerFnError::new)?
        {
            return Err(ServerFnError::new("conversation not found"));
        }

        let mut persisted = Vec::new();
        // Tracks what's been persisted since the last *real* Anthropic
        // response — the compaction trigger's cheap size estimate (see
        // `should_compact`) is computed over exactly this, added to that
        // last response's own real `usage`. Reset to empty every time a
        // real response lands (below); grows with the new user message,
        // any drained notifications, and (on a later loop iteration) the
        // previous turn's tool-result batch.
        let mut pending_new_content: Vec<anthropic::ContentBlock> = Vec::new();
        let mut last_known_usage = db::get_conversation_usage(pool, conversation_id)
            .await
            .map_err(ServerFnError::new)?;
        if let Some(new_message) = &new_message {
            let saved = db::create_message(
                pool,
                conversation_id,
                &new_message.role,
                &new_message.content,
            )
            .await
            .map_err(ServerFnError::new)?;
            pending_new_content.extend(new_message.content.clone());
            record_saved(conversation_id, &mut persisted, saved);
        }

        // Checked after saving the new message, so what the user typed
        // survives a reload even when the turn can't run.
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        let auth_token = std::env::var("ANTHROPIC_AUTH_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        require_at_least_one_credential(&api_key, &auth_token).map_err(ServerFnError::new)?;

        for _ in 0..max_turns {
            // Checked at the top of every loop iteration, not just once per
            // `run_turn` call — this is what gives same-turn visibility: if
            // a command finishes partway through a turn's tool-calling
            // loop, the very next iteration already sees the notification,
            // without waiting for a fresh user message. See the plan's
            // "What" and "How" (the completion-notification design).
            drain_unnotified_terminal_commands(
                pool,
                conversation_id,
                &mut pending_new_content,
                &mut persisted,
            )
            .await?;

            // Nothing to do: `new_message` was `None` (a pure "check for a
            // backlog" wake-up, see `wake_conversation`) and the drain
            // above found nothing pending either — e.g. another concurrent
            // wake, or an unrelated live message, already handled it. No
            // message to send and no reason to call the model, so this
            // returns before ever building a request. Only possible on the
            // very first iteration: every later one already has a real
            // tool_use turn's results to send regardless.
            if new_message.is_none() && persisted.is_empty() {
                return Ok(persisted);
            }

            // Proactive, not reactive: checked *before* building the
            // request that would be too big, using the last real
            // response's own usage plus a cheap estimate of what's new
            // since then — see `should_compact`/`estimate_tokens`. A
            // failure here fails the whole turn loudly rather than risking
            // the oversized request compaction exists to prevent — see
            // docs/projects/plans/auto-compaction.md's "Resolved" decisions.
            if should_compact(
                last_known_usage.as_ref(),
                estimate_tokens(&pending_new_content),
                context_window(),
            ) {
                compact_conversation(
                    pool,
                    conversation_id,
                    api_key.as_deref(),
                    auth_token.as_deref(),
                )
                .await?;
            }

            let history = history_for_request(
                db::list_messages(pool, conversation_id)
                    .await
                    .map_err(ServerFnError::new)?,
            )
            .map_err(ServerFnError::new)?;

            let mut request = anthropic::CreateMessageRequest {
                model: anthropic_model(),
                // Raised alongside `thinking`: adaptive thinking shares
                // this budget with the actual reply, and 4096 left no
                // headroom for both once thinking turned on.
                max_tokens: MAX_TOKENS,
                system: Some(system_prompt(&prompt_environment(pool).await)),
                messages: history,
                stream: true,
                tools: anthropic::tools::tool_definitions(pool).await,
                thinking: thinking_enabled().then_some(anthropic::ThinkingConfig::Adaptive),
            };

            // Every tab watching streams the reply: the text so far is kept
            // for a tab that connects mid-reply, and each delta published.
            let mut relay = |delta: &str| {
                REPLIES_IN_PROGRESS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(conversation_id)
                    .or_default()
                    .push_str(delta);
                crate::events::publish(
                    conversation_id,
                    crate::events::ConversationEvent::ReplyDelta {
                        text: delta.to_string(),
                    },
                );
                if let Some(cb) = on_delta.as_deref_mut() {
                    cb(delta);
                }
            };

            // See `is_ollama_thinking_tool_call_corruption` — not always
            // caused by thinking specifically (a local model can just
            // flub a tool call's JSON on its own, e.g. writing a bare `?`
            // for a value it wasn't sure about), so the mitigation is two
            // parts: drop `thinking` once (a plausible contributing
            // factor, and free to try), then fall back to plain retries —
            // regenerating is often enough on its own since sampling
            // varies run to run. Bounded so a model that's reliably bad at
            // one particular call can't loop forever.
            let mut turn = None;
            let mut last_err = String::new();
            for attempt in 0..=TOOL_CALL_PARSE_RETRIES {
                // A fresh bubble per model call, and per retry: a retry's
                // text replaces a failed attempt's.
                clear_reply_in_progress(conversation_id);
                crate::events::publish(
                    conversation_id,
                    crate::events::ConversationEvent::ReplyReset {},
                );
                match anthropic::stream::stream_anthropic_message(
                    api_key.as_deref(),
                    auth_token.as_deref(),
                    &request,
                    &mut relay,
                )
                .await
                {
                    Ok(t) => {
                        turn = Some(t);
                        break;
                    }
                    Err(e)
                        if attempt < TOOL_CALL_PARSE_RETRIES
                            && is_ollama_thinking_tool_call_corruption(&e) =>
                    {
                        request.thinking = None;
                        last_err = e;
                    }
                    Err(e) => return Err(ServerFnError::new(e)),
                }
            }
            let turn = turn.ok_or_else(|| ServerFnError::new(last_err))?;

            let saved = db::create_message(pool, conversation_id, "assistant", &turn.content)
                .await
                .map_err(ServerFnError::new)?;
            // Saved (and about to be published as a message), so no longer
            // "in progress".
            clear_reply_in_progress(conversation_id);
            record_saved(conversation_id, &mut persisted, saved);

            // Real usage from this call is the ground truth for "how much
            // context is actually being used" — persisted so
            // `get_context_usage` survives a reload, published live so the
            // indicator updates without one, and tracked here as the new
            // baseline the *next* compaction check starts from (everything
            // up through this response is now accounted for, so
            // `pending_new_content` resets — only what's persisted after
            // this point is "new" again). See
            // docs/projects/plans/auto-compaction.md.
            db::upsert_conversation_usage(pool, conversation_id, &turn.usage)
                .await
                .map_err(ServerFnError::new)?;
            crate::events::publish(
                conversation_id,
                crate::events::ConversationEvent::ContextUsageUpdate {
                    usage: turn.usage,
                    context_window: context_window(),
                },
            );
            last_known_usage = Some(turn.usage);
            pending_new_content.clear();

            if turn.stop_reason != "tool_use" {
                return Ok(persisted);
            }

            let mut result_blocks = Vec::new();
            for block in &turn.content {
                if let anthropic::ContentBlock::ToolUse { id, name, input } = block {
                    let result =
                        anthropic::tools::execute(pool, conversation_id, id, name, input).await;
                    let (content, is_error) = match result {
                        Ok(output) => (output, None),
                        Err(message) => (message, Some(true)),
                    };
                    result_blocks.push(anthropic::ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content,
                        is_error,
                    });
                }
            }

            let saved = db::create_message(pool, conversation_id, "user", &result_blocks)
                .await
                .map_err(ServerFnError::new)?;
            pending_new_content = result_blocks;
            record_saved(conversation_id, &mut persisted, saved);
        }

        Err(ServerFnError::new(format!(
            "tool-use loop exceeded {max_turns} turns without reaching a final reply"
        )))
    })
}

/// What the chat shows when a turn fails: the server's own message,
/// without the "error running server function: … (details: None)" wrapper
/// `ServerFnError`'s `Display` adds.
#[cfg(feature = "server")]
pub(crate) fn chat_error_text(error: &ServerFnError) -> String {
    match error {
        ServerFnError::ServerError { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

/// Sends the user's message: starts their turn and returns, without
/// holding a connection open for the reply. See `start_turn`.
#[post("/api/conversations/{id}/messages")]
pub async fn send_message(id: i64, content: String) -> ServerFnResult<()> {
    start_turn(db::get().clone(), id, content).await
}

/// The reply streamed so far in this conversation's current model call,
/// if any: part of the reconnect pull, so a tab that (re)connects
/// mid-reply shows the text so far.
#[get("/api/conversations/{id}/reply")]
pub async fn get_reply_in_progress(id: i64) -> ServerFnResult<Option<String>> {
    Ok(reply_in_progress(id))
}

/// Thin wrapper around `anthropic::tools::snapshot_tasks` for the browser —
/// a one-shot pull, not a subscription. Used both for the initial task-panel
/// load and for the reconciliation pull `subscribe_conversation_events`'s
/// caller does on connect/reconnect (a `broadcast` channel has no replay).
#[get("/api/conversations/{id}/tasks")]
pub async fn get_tasks(id: i64) -> ServerFnResult<Vec<anthropic::tools::TaskSummary>> {
    Ok(anthropic::tools::snapshot_tasks(id))
}

/// One-shot pull of the current todo list for the browser — same shape as
/// `get_tasks`, used for the todo panel's initial load and the
/// reconciliation pull on connect/reconnect.
#[get("/api/conversations/{id}/todos")]
pub async fn get_todos(id: i64) -> ServerFnResult<Vec<anthropic::tools::TodoItem>> {
    db::get_conversation_todos(db::get(), id)
        .await
        .map_err(ServerFnError::new)
}

/// One line of a command's output, in the order it actually happened —
/// `stdout`/`stderr` fetched and capped independently (see
/// `fetch_command_summary`) but merged back into one true chronological
/// sequence here, rather than the panel showing "all stdout, then all
/// stderr" the way two separate fields would.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SandboxOutputLine {
    pub stream: String,
    pub data: String,
}

/// One terminal's current/most recent command, hydrated for the sandbox
/// panel's initial scrollback — see `docs/projects/completed/20260815-sandbox-visibility.md`.
/// Only the current/most recent command is included; older history in a
/// terminal stays reachable through the model's own `list_commands`/
/// `read_terminal_output` tools, not duplicated here.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SandboxCommandSummary {
    pub command_id: String,
    pub command: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub output: Vec<SandboxOutputLine>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SandboxTerminalSummary {
    pub terminal_id: i64,
    pub pod_id: i64,
    pub status: String,
    /// Most recent `HISTORY_LIMIT` commands, **oldest first** — natural
    /// terminal-scrollback reading order, the newest command (and its
    /// output) at the bottom, closest to where the next command will
    /// appear. `db::list_terminal_commands` itself returns most-recent-
    /// first; this is reversed when the snapshot is built. Older commands
    /// beyond the limit stay reachable through the model's own
    /// `list_commands`/`read_terminal_output` tools, not duplicated here.
    pub commands: Vec<SandboxCommandSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SandboxPodSummary {
    pub pod_id: i64,
    pub status: String,
    pub terminals: Vec<SandboxTerminalSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SandboxSnapshot {
    pub pods: Vec<SandboxPodSummary>,
}

/// Last N lines per stream, per command, on initial load, matching
/// `read_terminal_output`'s own tool-facing default — the panel grows from
/// there via live events for as long as the tab stays open, never
/// re-fetching the full history.
#[cfg(feature = "server")]
const SNAPSHOT_TAIL_LINES: i64 = 200;

/// How many of a terminal's most recent commands the panel hydrates on
/// load — matches `list_commands`' own tool-facing default (see
/// `anthropic::tools::DEFAULT_LIST_COMMANDS_LIMIT`).
#[cfg(feature = "server")]
const HISTORY_LIMIT: i64 = 20;

#[cfg(feature = "server")]
async fn fetch_command_summary(
    pool: &PgPool,
    command: &db::TerminalCommand,
) -> Option<SandboxCommandSummary> {
    let status = db::terminal_command_status(pool, &command.command_id)
        .await
        .ok()??;
    let stdout_offset = (status.stdout_lines - SNAPSHOT_TAIL_LINES).max(0);
    let stderr_offset = (status.stderr_lines - SNAPSHOT_TAIL_LINES).max(0);
    let stdout = db::read_terminal_output(
        pool,
        &command.command_id,
        &["stdout"],
        stdout_offset,
        SNAPSHOT_TAIL_LINES,
    )
    .await
    .unwrap_or_default();
    let stderr = db::read_terminal_output(
        pool,
        &command.command_id,
        &["stderr"],
        stderr_offset,
        SNAPSHOT_TAIL_LINES,
    )
    .await
    .unwrap_or_default();
    // stdout/stderr are each capped to their own tail independently (so a
    // stderr spew can't crowd stdout out of the window, or vice versa),
    // which means they arrive as two separately-ordered lists — merge back
    // by `seq` into the order the lines actually happened in.
    let mut output: Vec<db::TerminalLine> = stdout.into_iter().chain(stderr).collect();
    output.sort_by_key(|line| line.seq);
    Some(SandboxCommandSummary {
        command_id: command.command_id.clone(),
        command: command.command.clone(),
        status: status.status,
        exit_code: status.exit_code,
        output: output
            .into_iter()
            .map(|line| SandboxOutputLine {
                stream: line.stream,
                data: line.data,
            })
            .collect(),
    })
}

#[cfg(feature = "server")]
async fn fetch_terminal_command_history(
    pool: &PgPool,
    terminal_id: i64,
) -> Vec<SandboxCommandSummary> {
    let Ok(recent) = db::list_terminal_commands(pool, terminal_id, HISTORY_LIMIT).await else {
        return Vec::new();
    };
    let mut summaries = Vec::with_capacity(recent.len());
    for command in recent.iter().rev() {
        if let Some(summary) = fetch_command_summary(pool, command).await {
            summaries.push(summary);
        }
    }
    summaries
}

/// Thin wrapper over `sandbox::list_pods`/`sandbox::list_terminals` for the
/// browser — a one-shot pull, not a subscription, same shape as `get_tasks`.
/// Used both for the initial sandbox-panel load and for the reconciliation
/// pull `subscribe_conversation_events`'s caller does on connect/reconnect.
///
/// Eagerly attempts `sandbox::try_reconnect` for every pod before reading
/// terminal status — otherwise a pod that survived a smelt restart with a
/// perfectly healthy agent still reports "disconnected" here until the
/// model happens to touch it next, since `list_terminals` itself only ever
/// checks the connection registry, never tries to repair it.
#[get("/api/conversations/{id}/sandbox")]
pub async fn get_sandbox_state(id: i64) -> ServerFnResult<SandboxSnapshot> {
    let pool = db::get();
    let pods = sandbox::list_pods(pool, id)
        .await
        .map_err(ServerFnError::new)?;
    for pod in &pods {
        sandbox::try_reconnect(pool, pod.pod_id).await;
    }
    let terminals = sandbox::list_terminals(pool, id)
        .await
        .map_err(ServerFnError::new)?;

    let mut by_pod: HashMap<i64, Vec<SandboxTerminalSummary>> = HashMap::new();
    for terminal in terminals {
        let commands = fetch_terminal_command_history(pool, terminal.terminal_id).await;
        by_pod
            .entry(terminal.pod_id)
            .or_default()
            .push(SandboxTerminalSummary {
                terminal_id: terminal.terminal_id,
                pod_id: terminal.pod_id,
                status: terminal.status,
                commands,
            });
    }

    let pods = pods
        .into_iter()
        .map(|pod| SandboxPodSummary {
            pod_id: pod.pod_id,
            status: pod.status,
            terminals: by_pod.remove(&pod.pod_id).unwrap_or_default(),
        })
        .collect();

    Ok(SandboxSnapshot { pods })
}

/// How full the model's context window is right now — the always-visible
/// indicator's data. `usage` is `None` for a conversation with no
/// completed turn yet (nothing to report). See
/// docs/projects/plans/auto-compaction.md.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ContextUsageSnapshot {
    pub usage: Option<anthropic::TokenUsage>,
    pub context_window: u32,
}

/// `context_window_for(&anthropic_model())`, falling back to
/// `ANTHROPIC_CONTEXT_WINDOW` (for a gateway/local model the built-in table
/// doesn't recognize), falling back again to a conservative default if
/// neither resolves it — see docs/projects/plans/auto-compaction.md's
/// "Resolved: the context window is looked up per-model."
#[cfg(feature = "server")]
fn context_window() -> u32 {
    crate::anthropic::context_window_for(&anthropic_model())
        .or_else(|| {
            std::env::var("ANTHROPIC_CONTEXT_WINDOW")
                .ok()
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(200_000)
}

/// One-shot pull for the always-visible context-usage indicator — same
/// shape as `get_tasks`/`get_sandbox_state`.
#[get("/api/conversations/{id}/context-usage")]
pub async fn get_context_usage(id: i64) -> ServerFnResult<ContextUsageSnapshot> {
    let usage = db::get_conversation_usage(db::get(), id)
        .await
        .map_err(ServerFnError::new)?;
    Ok(ContextUsageSnapshot {
        usage,
        context_window: context_window(),
    })
}

/// The click-through detail view: every available tool's full definition,
/// the current message count, and the same usage numbers
/// `get_context_usage` reports — reconstructed from current state rather
/// than a stored snapshot of what was literally sent (matches
/// `run_turn_bounded`'s own request-building exactly, since nothing else
/// changes `system`/the tool list between turns). `system` comes from
/// the same `system_prompt`/`prompt_environment` pair a turn uses.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ContextDetailSnapshot {
    pub system: Option<String>,
    pub tools: Vec<anthropic::ToolDefinition>,
    pub message_count: usize,
    pub usage: Option<anthropic::TokenUsage>,
    pub context_window: u32,
}

#[get("/api/conversations/{id}/context-detail")]
pub async fn get_context_detail(id: i64) -> ServerFnResult<ContextDetailSnapshot> {
    context_detail(db::get(), id).await
}

/// `get_context_detail`'s body, taking the pool so tests can call it.
#[cfg(feature = "server")]
async fn context_detail(pool: &PgPool, id: i64) -> ServerFnResult<ContextDetailSnapshot> {
    let tools = anthropic::tools::tool_definitions(pool).await;
    let message_count = db::list_messages(pool, id)
        .await
        .map_err(ServerFnError::new)?
        .len();
    let usage = db::get_conversation_usage(pool, id)
        .await
        .map_err(ServerFnError::new)?;
    Ok(ContextDetailSnapshot {
        system: Some(system_prompt(&prompt_environment(pool).await)),
        tools,
        message_count,
        usage,
        context_window: context_window(),
    })
}

/// A dedicated, always-open per-conversation event stream — independent of
/// any particular `send_message` call, since task activity (a tick, a
/// finish) or another writer's pushed turn can happen with no request in
/// flight at all. The frontend opens this once per viewed conversation and
/// keeps it open for as long as that conversation is selected; see
/// `docs/architecture.md` for why this needs its own stream rather than
/// reusing `send_message`'s.
#[get("/api/conversations/{id}/events")]
pub async fn subscribe_conversation_events(
    id: i64,
) -> ServerFnResult<ServerEvents<events::ConversationEvent>> {
    // `from_stream`, not `ServerEvents::new`: `new` runs its loop as a
    // detached task that never notices the connection closing, so every
    // page load or reconnect used to leave a subscriber behind for good.
    // Here the response pulls events as it sends them, and dropping it
    // (the tab going away) drops the subscription.
    Ok(ServerEvents::from_stream(conversation_event_stream(id)))
}

/// Everything a tab watching `id` hears: that conversation's events, plus
/// app-wide `PodsChanged`, relayed as `ConversationEvent::PodsChanged`.
/// Ends when the conversation's channel closes (it was deleted).
#[cfg(feature = "server")]
fn conversation_event_stream(
    id: i64,
) -> impl futures_util::Stream<Item = Result<events::ConversationEvent, axum::BoxError>> {
    use tokio::sync::broadcast::error::RecvError;
    // Both subscribed now, not on first poll, so nothing published in
    // between is missed.
    let receivers = (events::subscribe(id), events::subscribe_app());
    futures_util::stream::unfold(receivers, |(mut conversation, mut app)| async move {
        loop {
            tokio::select! {
                received = conversation.recv() => match received {
                    Ok(event) => return Some((Ok::<_, axum::BoxError>(event), (conversation, app))),
                    // A subscriber that fell behind just misses some
                    // ephemeral updates — the frontend's reconciliation
                    // pull on connect covers the durable state regardless.
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return None,
                },
                received = app.recv() => match received {
                    Ok(events::AppEvent::PodsChanged) => {
                        let event = events::ConversationEvent::PodsChanged {};
                        return Some((Ok(event), (conversation, app)));
                    }
                    // Missing some just means one refetch covers several.
                    Err(RecvError::Lagged(_)) => continue,
                    // The app-wide channel lives as long as the process.
                    Err(RecvError::Closed) => return None,
                },
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn message_with_blocks(id: i64, role: &str, blocks: Vec<anthropic::ContentBlock>) -> Message {
        Message {
            id,
            conversation_id: 1,
            role: role.to_string(),
            content: serde_json::to_string(&blocks).expect("ContentBlock always serializes"),
            created_at: chrono::Utc::now().naive_utc(),
        }
    }

    fn text_message(id: i64, role: &str, text: &str) -> Message {
        message_with_blocks(id, role, vec![text_block(text)])
    }

    /// Messages 1-2 were already summarized by an earlier compaction
    /// (3-5); only 6 is new since.
    fn compacted_once() -> Vec<Message> {
        let mut messages = vec![
            text_message(1, "user", "ancient question"),
            text_message(2, "assistant", "ancient answer"),
        ];
        for (offset, (role, blocks)) in compaction_messages("the earlier summary".to_string(), 2)
            .into_iter()
            .enumerate()
        {
            messages.push(message_with_blocks(3 + offset as i64, role, blocks));
        }
        messages.push(text_message(6, "user", "recent question"));
        messages
    }

    #[test]
    fn test_compaction_transcript_starts_from_the_latest_summary() {
        let transcript = compaction_transcript(&compacted_once(), usize::MAX);
        assert!(transcript.contains("the earlier summary"), "got {transcript}");
        assert!(transcript.contains("recent question"), "got {transcript}");
        assert!(
            !transcript.contains("ancient"),
            "messages an earlier compaction already replaced came back: {transcript}"
        );
    }

    #[test]
    fn test_compaction_transcript_keeps_the_most_recent_part_within_its_budget() {
        let mut messages: Vec<Message> = (1..=50)
            .map(|id| text_message(id, "user", &format!("message {id} {}", "x".repeat(1_000))))
            .collect();
        messages.push(text_message(51, "user", "the very latest"));
        let transcript = compaction_transcript(&messages, 5_000);
        assert!(transcript.chars().count() <= 5_000, "{} chars", transcript.chars().count());
        assert!(transcript.contains("the very latest"));
        assert!(transcript.contains("omitted"), "the cut should be marked");
        assert!(!transcript.contains("message 1 "), "the oldest part should be what's dropped");
    }

    #[test]
    fn test_compaction_messages_start_with_user_and_alternate_correctly() {
        let inserted = compaction_messages("the summary".to_string(), 42);
        let roles: Vec<&str> = inserted.iter().map(|(role, _)| *role).collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "user"],
            "Anthropic requires messages to start with user and strictly \
             alternate — a single summary message can't satisfy both \
             'starts with user' and 'ends with something to respond to'"
        );
        assert_eq!(
            inserted[1].1,
            vec![anthropic::ContentBlock::CompactionSummary {
                summary: "the summary".to_string(),
                covers_through_message_id: 42,
            }]
        );
    }

    fn text_block(text: &str) -> anthropic::ContentBlock {
        anthropic::ContentBlock::Text {
            text: text.to_string(),
        }
    }

    fn tool_use(id: &str) -> anthropic::ContentBlock {
        anthropic::ContentBlock::ToolUse {
            id: id.to_string(),
            name: "add".to_string(),
            input: serde_json::json!({}),
        }
    }

    fn tool_result(id: &str) -> anthropic::ContentBlock {
        anthropic::ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: "3".to_string(),
            is_error: None,
        }
    }

    fn unfinished(id: &str) -> anthropic::ContentBlock {
        anthropic::ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: UNFINISHED_TOOL_CALL.to_string(),
            is_error: Some(true),
        }
    }

    /// A turn stopped (or a server restarted) between a tool call and its
    /// result leaves the call unanswered, and a notice may have landed
    /// after it. The request gets an error result for it, at the start of
    /// the next user message.
    #[test]
    fn test_history_for_request_answers_a_tool_call_left_without_a_result() {
        let history = history_for_request(vec![
            text_message(1, "user", "go"),
            message_with_blocks(2, "assistant", vec![tool_use("t1")]),
            text_message(3, "user", "notice"),
        ])
        .expect("history");
        assert_eq!(history.len(), 3);
        assert_eq!(history[2].role, "user");
        assert_eq!(history[2].content, vec![unfinished("t1"), text_block("notice")]);
    }

    #[test]
    fn test_history_for_request_answers_only_the_missing_calls_of_several() {
        let history = history_for_request(vec![
            text_message(1, "user", "go"),
            message_with_blocks(2, "assistant", vec![tool_use("t1"), tool_use("t2")]),
            message_with_blocks(3, "user", vec![tool_result("t1")]),
        ])
        .expect("history");
        assert_eq!(history[2].content, vec![unfinished("t2"), tool_result("t1")]);
    }

    #[test]
    fn test_history_for_request_adds_a_message_for_calls_left_at_the_end() {
        let history = history_for_request(vec![
            text_message(1, "user", "go"),
            message_with_blocks(2, "assistant", vec![text_block("on it"), tool_use("t1")]),
        ])
        .expect("history");
        assert_eq!(history.len(), 3);
        assert_eq!(history[2].role, "user");
        assert_eq!(history[2].content, vec![unfinished("t1")]);
    }

    #[test]
    fn test_history_for_request_passes_everything_through_unchanged_with_no_compaction() {
        let messages = vec![
            message_with_blocks(1, "user", vec![text_block("hi")]),
            message_with_blocks(2, "assistant", vec![text_block("hello")]),
        ];
        let history = history_for_request(messages).expect("should parse");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, "user");
        assert_eq!(history[0].content, vec![text_block("hi")]);
        assert_eq!(history[1].role, "assistant");
        assert_eq!(history[1].content, vec![text_block("hello")]);
    }

    #[test]
    fn test_history_for_request_skips_covered_messages_and_translates_the_summary() {
        let messages = vec![
            message_with_blocks(1, "user", vec![text_block("old message 1")]),
            message_with_blocks(2, "assistant", vec![text_block("old reply 1")]),
            message_with_blocks(
                3,
                "user",
                vec![anthropic::ContentBlock::CompactionSummary {
                    summary: "condensed: talked about X".to_string(),
                    covers_through_message_id: 2,
                }],
            ),
            message_with_blocks(4, "user", vec![text_block("new message")]),
        ];
        let history = history_for_request(messages).expect("should parse");
        assert_eq!(
            history,
            vec![
                anthropic::AnthropicMessage {
                    role: "user".to_string(),
                    content: vec![text_block("condensed: talked about X")],
                },
                anthropic::AnthropicMessage {
                    role: "user".to_string(),
                    content: vec![text_block("new message")],
                },
            ]
        );
    }

    #[test]
    fn test_history_for_request_translates_compaction_placeholder_to_text_too() {
        // Anthropic has no concept of either synthetic block type — an
        // untranslated CompactionPlaceholder forwarded as-is would be
        // rejected outright, same reasoning as CompactionSummary above.
        let messages = vec![message_with_blocks(
            1,
            "user",
            vec![anthropic::ContentBlock::CompactionPlaceholder {
                text: "Continue based on the summary above.".to_string(),
            }],
        )];
        let history = history_for_request(messages).expect("should parse");
        assert_eq!(
            history,
            vec![anthropic::AnthropicMessage {
                role: "user".to_string(),
                content: vec![text_block("Continue based on the summary above.")],
            }]
        );
    }

    #[test]
    fn test_history_for_request_only_the_latest_of_two_compactions_applies() {
        let messages = vec![
            message_with_blocks(1, "user", vec![text_block("ancient")]),
            message_with_blocks(
                2,
                "user",
                vec![anthropic::ContentBlock::CompactionSummary {
                    summary: "first summary".to_string(),
                    covers_through_message_id: 1,
                }],
            ),
            message_with_blocks(3, "user", vec![text_block("middle")]),
            message_with_blocks(
                4,
                "user",
                vec![anthropic::ContentBlock::CompactionSummary {
                    summary: "second summary, supersedes the first".to_string(),
                    covers_through_message_id: 3,
                }],
            ),
            message_with_blocks(5, "user", vec![text_block("recent")]),
        ];
        let history = history_for_request(messages).expect("should parse");
        // Message 2 (the first compaction's own summary) has id 2 <= the
        // second compaction's boundary (3), so it's superseded and skipped
        // entirely too — only the second summary and what came after it
        // survive.
        assert_eq!(
            history,
            vec![
                anthropic::AnthropicMessage {
                    role: "user".to_string(),
                    content: vec![text_block("second summary, supersedes the first")],
                },
                anthropic::AnthropicMessage {
                    role: "user".to_string(),
                    content: vec![text_block("recent")],
                },
            ]
        );
    }

    fn usage(
        input: i64,
        output: i64,
        cache_creation: i64,
        cache_read: i64,
    ) -> anthropic::TokenUsage {
        anthropic::TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_creation_input_tokens: cache_creation,
            cache_read_input_tokens: cache_read,
        }
    }

    #[test]
    fn test_estimate_tokens_uses_chars_over_4_heuristic() {
        let blocks = vec![anthropic::ContentBlock::Text {
            text: "a".repeat(400),
        }];
        assert_eq!(estimate_tokens(&blocks), 100);
    }

    #[test]
    fn test_estimate_tokens_sums_every_block_and_every_category() {
        let blocks = vec![
            anthropic::ContentBlock::Text {
                text: "a".repeat(40),
            },
            anthropic::ContentBlock::ToolResult {
                tool_use_id: "t1".to_string(),
                content: "b".repeat(40),
                is_error: None,
            },
        ];
        assert_eq!(estimate_tokens(&blocks), 20);
    }

    #[test]
    fn test_should_compact_false_when_nothing_sent_yet() {
        assert!(!should_compact(None, 1000, 200_000));
    }

    #[test]
    fn test_should_compact_false_comfortably_under_ceiling() {
        let last = usage(10_000, 2_000, 0, 0);
        assert!(!should_compact(Some(&last), 500, 200_000));
    }

    #[test]
    fn test_should_compact_true_when_projected_crosses_reserved_ceiling() {
        // ceiling = 200_000 - max(MAX_TOKENS, COMPACTION_SAFETY_BUFFER) = 183_616
        let last = usage(183_000, 0, 0, 0);
        assert!(should_compact(Some(&last), 1_000, 200_000));
    }

    #[test]
    fn test_should_compact_counts_cache_tokens_in_already_used() {
        let last = usage(0, 0, 90_000, 90_000);
        assert!(should_compact(Some(&last), 10_000, 200_000));
    }

    #[test]
    fn test_is_safe_compaction_boundary_true_for_plain_text() {
        let blocks = vec![anthropic::ContentBlock::Text {
            text: "hi".to_string(),
        }];
        assert!(is_safe_compaction_boundary(&blocks));
    }

    #[test]
    fn test_is_safe_compaction_boundary_true_for_tool_result_only() {
        let blocks = vec![anthropic::ContentBlock::ToolResult {
            tool_use_id: "t1".to_string(),
            content: "3".to_string(),
            is_error: None,
        }];
        assert!(is_safe_compaction_boundary(&blocks));
    }

    #[test]
    fn test_forget_conversation_lock_drops_it() {
        let conversation_id = 987_654_011;
        let first = conversation_lock(conversation_id);
        let _ = stop_receiver(conversation_id);
        forget_conversation_lock(conversation_id);
        assert!(
            !TURN_STOPS.lock().expect("stops").contains_key(&conversation_id),
            "the deleted conversation's stop signal is still registered"
        );
        let second = conversation_lock(conversation_id);
        assert!(
            !Arc::ptr_eq(&first, &second),
            "the deleted conversation's lock is still registered"
        );
        forget_conversation_lock(conversation_id);
    }

    /// A tab watching a conversation also hears app-wide pod changes, on
    /// the same stream.
    #[tokio::test]
    async fn test_a_conversation_stream_relays_pods_changed() {
        use futures_util::StreamExt;
        let stream = conversation_event_stream(9_000_000_011);
        futures_util::pin_mut!(stream);
        events::publish_app(events::AppEvent::PodsChanged);
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
            .await
            .expect("PodsChanged should arrive on the conversation stream");
        assert!(
            matches!(event, Some(Ok(events::ConversationEvent::PodsChanged {}))),
            "got {event:?}"
        );
    }

    /// A subscription belongs to its connection: once the response is
    /// dropped (the tab closed or reloaded), nothing should still be
    /// listening on the conversation's channel.
    #[tokio::test]
    async fn test_a_dropped_event_subscription_stops_listening() {
        let conversation_id = 9_000_000_007;
        let subscription = subscribe_conversation_events(conversation_id)
            .await
            .expect("subscribe");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(events::subscriber_count(conversation_id), 1);
        drop(subscription);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            events::subscriber_count(conversation_id),
            0,
            "a dropped subscription is still listening"
        );
    }

    #[test]
    fn test_is_safe_compaction_boundary_false_when_tool_use_present() {
        let blocks = vec![anthropic::ContentBlock::ToolUse {
            id: "t1".to_string(),
            name: "add".to_string(),
            input: serde_json::json!({}),
        }];
        assert!(!is_safe_compaction_boundary(&blocks));
    }

    /// Spins up a mock Anthropic upstream that returns `bodies` in order (one
    /// per request, clamped to the last body once exhausted) and points
    /// `ANTHROPIC_BASE_URL`/`ANTHROPIC_API_KEY` (both process-global) at it.
    /// Callers must hold `anthropic::test_support::lock_anthropic_base_url`
    /// for the duration, same as `anthropic::stream`'s own mock-upstream
    /// tests.
    async fn start_mock_upstream(bodies: Vec<String>) -> Arc<AtomicUsize> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        let bodies = Arc::new(bodies);
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_route = counter.clone();
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move || {
                let bodies = bodies.clone();
                let counter = counter_for_route.clone();
                async move {
                    let i = counter.fetch_add(1, Ordering::SeqCst);
                    let body = bodies[i.min(bodies.len() - 1)].clone();
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        body,
                    )
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        unsafe {
            std::env::set_var("ANTHROPIC_BASE_URL", format!("http://{addr}"));
            std::env::set_var("ANTHROPIC_API_KEY", "test-key");
        }
        counter
    }

    fn sse_body(events: &[(&str, &str)]) -> String {
        events
            .iter()
            .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
            .collect()
    }

    /// Like `start_mock_upstream`, but the first `fail_count` requests get
    /// back a flat HTTP 500 with Ollama's real "error parsing tool call"
    /// body — the exact shape `is_ollama_thinking_tool_call_corruption` is
    /// meant to recognize — and every request after that gets
    /// `success_body` (a normal 200 SSE stream), if any. `success_body:
    /// None` means every request fails, for testing the give-up path once
    /// `TOOL_CALL_PARSE_RETRIES` is exhausted.
    async fn start_mock_upstream_failing_n_times(fail_count: usize, success_body: Option<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        let counter = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move || {
                let counter = counter.clone();
                let success_body = success_body.clone();
                async move {
                    let i = counter.fetch_add(1, Ordering::SeqCst);
                    if i < fail_count || success_body.is_none() {
                        (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            format!(
                                r#"{{"type":"error","error":{{"type":"api_error","message":"error parsing tool call: raw='attempt {i}' err=invalid character '?' after object key:value pair"}},"request_id":"req_test"}}"#
                            ),
                        )
                            .into_response()
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                            success_body.expect("checked above"),
                        )
                            .into_response()
                    }
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        unsafe {
            std::env::set_var("ANTHROPIC_BASE_URL", format!("http://{addr}"));
            std::env::set_var("ANTHROPIC_API_KEY", "test-key");
        }
    }

    /// `ANTHROPIC_THINKING` is process-global like `ANTHROPIC_BASE_URL`, but
    /// unlike it, no other test reads `thinking_enabled()`'s result, so
    /// nothing else can fail if a concurrently-running test transiently
    /// observes this one's value — restoring it afterward (rather than a
    /// dedicated lock) is enough.
    #[test]
    fn test_thinking_enabled_defaults_to_on_and_recognizes_opt_out_values() {
        // `ANTHROPIC_THINKING` is process-global, same as `ANTHROPIC_BASE_URL`
        // — reusing that lock (rather than a dedicated one) keeps this
        // mutually exclusive with `test_run_turn_retries_without_thinking_...`,
        // which needs the *default* (on) to actually exercise the retry path.
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let original = std::env::var("ANTHROPIC_THINKING").ok();

        unsafe { std::env::remove_var("ANTHROPIC_THINKING") };
        assert!(
            thinking_enabled(),
            "should default to on — see the doc comment for why"
        );

        for value in ["0", "false", "off"] {
            unsafe { std::env::set_var("ANTHROPIC_THINKING", value) };
            assert!(!thinking_enabled(), "{value:?} should disable thinking");
        }

        for value in ["1", "true", "on", "nonsense"] {
            unsafe { std::env::set_var("ANTHROPIC_THINKING", value) };
            assert!(thinking_enabled(), "{value:?} should not disable thinking");
        }

        match original {
            Some(value) => unsafe { std::env::set_var("ANTHROPIC_THINKING", value) },
            None => unsafe { std::env::remove_var("ANTHROPIC_THINKING") },
        }
    }

    #[test]
    fn test_is_ollama_thinking_tool_call_corruption_matches_the_known_error_shape() {
        let real_error = r#"Anthropic API error 500 Internal Server Error: {"type":"error","error":{"type":"api_error","message":"error parsing tool call: raw='...' err=invalid character 'T' looking for beginning of value"},"request_id":"req_123"}"#;
        assert!(is_ollama_thinking_tool_call_corruption(real_error));
    }

    #[test]
    fn test_is_ollama_thinking_tool_call_corruption_does_not_match_unrelated_errors() {
        assert!(!is_ollama_thinking_tool_call_corruption(
            "Anthropic API error 529 Overloaded: the server is overloaded"
        ));
        assert!(!is_ollama_thinking_tool_call_corruption(
            "timed out waiting for Anthropic to respond"
        ));
    }

    /// `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN` are process-global, same
    /// as `ANTHROPIC_BASE_URL` — restoring both afterward (rather than a
    /// dedicated lock) is enough, same reasoning
    /// `test_thinking_enabled_defaults_to_on_and_recognizes_opt_out_values`
    /// already applies to `ANTHROPIC_THINKING`. Callers must still hold
    /// `lock_anthropic_base_url` for the duration, since this also touches
    /// `ANTHROPIC_BASE_URL`-adjacent test infrastructure other tests share.
    struct CredentialEnvGuard {
        original_api_key: Option<String>,
        original_auth_token: Option<String>,
    }

    impl CredentialEnvGuard {
        fn capture() -> Self {
            Self {
                original_api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
                original_auth_token: std::env::var("ANTHROPIC_AUTH_TOKEN").ok(),
            }
        }
    }

    impl Drop for CredentialEnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.original_api_key {
                    Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
                    None => std::env::remove_var("ANTHROPIC_API_KEY"),
                }
                match &self.original_auth_token {
                    Some(v) => std::env::set_var("ANTHROPIC_AUTH_TOKEN", v),
                    None => std::env::remove_var("ANTHROPIC_AUTH_TOKEN"),
                }
            }
        }
    }

    #[test]
    fn test_require_at_least_one_credential_errors_when_both_are_missing() {
        let err = require_at_least_one_credential(&None, &None)
            .expect_err("should error when neither credential is set");
        assert!(
            err.contains("ANTHROPIC_API_KEY") && err.contains("ANTHROPIC_AUTH_TOKEN"),
            "expected the error to name both env vars, got: {err}"
        );
    }

    #[test]
    fn test_require_at_least_one_credential_allows_api_key_only() {
        require_at_least_one_credential(&Some("sk-ant-...".to_string()), &None)
            .expect("an API key alone should be sufficient");
    }

    #[test]
    fn test_require_at_least_one_credential_allows_auth_token_only() {
        require_at_least_one_credential(&None, &Some("hf-token".to_string())).expect(
            "an auth token alone should be sufficient — e.g. a Hugging Face-hosted endpoint",
        );
    }

    #[test]
    fn test_require_at_least_one_credential_allows_both_present() {
        require_at_least_one_credential(
            &Some("sk-ant-...".to_string()),
            &Some("hf-token".to_string()),
        )
        .expect("both being present should still be fine");
    }

    #[sqlx::test]
    async fn test_run_turn_succeeds_with_only_auth_token_set_no_api_key(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let _env_guard = CredentialEnvGuard::capture();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        let body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi!"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![body]).await;
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::set_var("ANTHROPIC_AUTH_TOKEN", "hf-token");
        }

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "hello".to_string(),
            }],
        };

        let messages = run_turn(&pool, conversation.id, new_message, None)
            .await
            .expect("run_turn should succeed using only ANTHROPIC_AUTH_TOKEN, with no ANTHROPIC_API_KEY set");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].role, "assistant");
    }

    /// A finished, not-yet-notified terminal command — the state
    /// `wake_conversation` is meant to react to. Mirrors `db.rs`'s own
    /// `test_terminal` helper shape.
    async fn unnotified_finished_command(pool: &PgPool, conversation_id: i64, command_id: &str) {
        let pod = db::create_sandbox_pod(pool, conversation_id)
            .await
            .expect("create sandbox pod");
        let terminal = db::create_sandbox_terminal(pool, pod.id)
            .await
            .expect("create sandbox terminal");
        db::create_terminal_command(pool, conversation_id, terminal.id, command_id, "echo hi")
            .await
            .expect("create terminal command");
        db::mark_terminal_command_finished(pool, command_id, 0)
            .await
            .expect("mark terminal command finished");
    }

    #[sqlx::test]
    async fn test_wake_conversation_is_a_noop_when_nothing_is_pending(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");
        let counter = start_mock_upstream(vec!["unused".to_string()]).await;

        let result = wake_conversation(&pool, conversation.id)
            .await
            .expect("wake_conversation should succeed even with nothing pending");
        assert!(
            result.is_empty(),
            "expected no persisted messages, got {result:?}"
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "nothing pending should mean no API call at all"
        );

        let messages = db::list_messages(&pool, conversation.id)
            .await
            .expect("list messages");
        assert!(
            messages.is_empty(),
            "no message should be persisted when nothing is pending"
        );
    }

    #[sqlx::test]
    async fn test_wake_conversation_drains_a_pending_command_and_completes_a_turn(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");
        unnotified_finished_command(&pool, conversation.id, "cmd-1").await;

        let body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Noted."}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![body]).await;

        let messages = wake_conversation(&pool, conversation.id)
            .await
            .expect("wake_conversation should succeed");

        assert_eq!(
            messages.len(),
            2,
            "expected the notification plus the assistant's reply, got {messages:?}"
        );
        assert_eq!(messages[0].role, "user");
        assert_eq!(
            messages[0].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::Text {
                text: "Terminal command cmd-1 finished: exit code 0.".to_string()
            }]
        );
        assert_eq!(messages[1].role, "assistant");

        let remaining = db::unnotified_finished_terminal_commands(&pool, conversation.id)
            .await
            .expect("query unnotified commands");
        assert!(
            remaining.is_empty(),
            "the command should now be marked notified"
        );
    }

    #[sqlx::test]
    async fn test_wake_conversation_second_call_is_a_noop_once_the_first_drained_everything(
        pool: PgPool,
    ) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");
        unnotified_finished_command(&pool, conversation.id, "cmd-1").await;

        let body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Noted."}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        let counter = start_mock_upstream(vec![body]).await;

        let first = wake_conversation(&pool, conversation.id)
            .await
            .expect("first wake should succeed");
        assert_eq!(first.len(), 2);
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "first wake should make exactly one API call"
        );

        // Simulates a second, near-simultaneous exit event's own detached
        // wake_conversation call — nothing should be left to drain, so this
        // must not persist another message or make another API call.
        let second = wake_conversation(&pool, conversation.id)
            .await
            .expect("second wake should succeed");
        assert!(
            second.is_empty(),
            "second wake should find nothing left to drain, got {second:?}"
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "second wake should not make another API call"
        );
    }

    #[sqlx::test]
    async fn test_wake_conversation_publishes_notification_delivery_failed_on_error(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");
        unnotified_finished_command(&pool, conversation.id, "cmd-1").await;

        // `fail_count` is irrelevant when `success_body` is `None` — every
        // request fails regardless (see the helper's doc comment).
        start_mock_upstream_failing_n_times(0, None).await;

        let mut rx = events::subscribe(conversation.id);

        let result = wake_conversation(&pool, conversation.id).await;
        assert!(
            result.is_err(),
            "expected wake_conversation to surface the underlying failure, got {result:?}"
        );

        // The turn publishes other events too (its state, the drained
        // notice, reply resets); wait for this one.
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match rx.recv().await.expect("event channel should not close") {
                    event @ events::ConversationEvent::NotificationDeliveryFailed { .. } => return event,
                    _ => continue,
                }
            }
        })
        .await
        .expect("should not time out waiting for the event");
        assert!(
            matches!(
                event,
                events::ConversationEvent::NotificationDeliveryFailed { .. }
            ),
            "expected NotificationDeliveryFailed, got {event:?}"
        );

        // The notification text itself is durably persisted regardless —
        // the drain step commits before the API call that failed.
        let messages = db::list_messages(&pool, conversation.id)
            .await
            .expect("list messages");
        assert_eq!(
            messages.len(),
            1,
            "the notification message should still be persisted, got {messages:?}"
        );
        assert_eq!(messages[0].role, "user");
    }

    #[test]
    fn test_chat_error_text_drops_the_server_function_wrapper() {
        let error = ServerFnError::new("model provider error 503 Service Unavailable: paused");
        assert_eq!(
            chat_error_text(&error),
            "model provider error 503 Service Unavailable: paused"
        );
    }

    fn prompt_env() -> PromptEnvironment {
        PromptEnvironment {
            date: chrono::NaiveDate::from_ymd_opt(2026, 9, 25).expect("a valid date"),
            model: "claude-test-model".to_string(),
            volumes: vec![("cargo-cache".to_string(), "/home/sandbox/.cargo".to_string())],
            mcp_servers: vec!["exa".to_string(), "github".to_string()],
        }
    }

    #[test]
    fn test_system_prompt_starts_with_the_base_prompt() {
        let prompt = system_prompt(&prompt_env());
        assert!(prompt.starts_with(BASE_SYSTEM_PROMPT), "got: {prompt}");
    }

    #[test]
    fn test_system_prompt_describes_the_environment() {
        let prompt = system_prompt(&prompt_env());
        let environment = prompt
            .strip_prefix(BASE_SYSTEM_PROMPT)
            .expect("the environment follows the base prompt");
        for expected in [
            "Today's date: 2026-09-25",
            "Model: claude-test-model",
            "cargo-cache mounted at /home/sandbox/.cargo",
            "MCP servers: exa, github",
        ] {
            assert!(environment.contains(expected), "missing {expected:?} in: {environment}");
        }
    }

    #[test]
    fn test_system_prompt_leaves_out_empty_volume_and_mcp_lines() {
        let mut env = prompt_env();
        env.volumes.clear();
        env.mcp_servers.clear();
        let prompt = system_prompt(&env);
        let environment = prompt
            .strip_prefix(BASE_SYSTEM_PROMPT)
            .expect("the environment follows the base prompt");
        assert!(environment.contains("Today's date: 2026-09-25"), "got: {environment}");
        assert!(!environment.contains("mounted at"), "got: {environment}");
        assert!(!environment.contains("MCP servers"), "got: {environment}");
    }

    /// Like `start_mock_upstream` (replies in order, the last one repeated),
    /// but also keeps every request body it receives, parsed as JSON, so a
    /// test can check what was actually sent. Same locking rule as
    /// `start_mock_upstream`.
    async fn start_recording_mock_upstream(
        bodies: Vec<String>,
    ) -> Arc<std::sync::Mutex<Vec<serde_json::Value>>> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move |request: String| {
                let recorded = recorded.clone();
                let bodies = bodies.clone();
                async move {
                    let parsed = serde_json::from_str(&request).expect("a JSON request body");
                    let mut log = recorded.lock().expect("the request log");
                    log.push(parsed);
                    let body = bodies[(log.len() - 1).min(bodies.len() - 1)].clone();
                    ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], body)
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        unsafe {
            std::env::set_var("ANTHROPIC_BASE_URL", format!("http://{addr}"));
            std::env::set_var("ANTHROPIC_API_KEY", "test-key");
        }
        requests
    }

    fn text_reply_body(text: &str) -> String {
        sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                &format!(
                    r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{text}"}}}}"#
                ),
            ),
            ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ])
    }

    #[sqlx::test]
    async fn test_prompt_environment_reads_volumes_and_mcp_servers(pool: PgPool) {
        db::create_sandbox_volume(&pool, "cargo-cache", "/home/sandbox/.cargo")
            .await
            .expect("create volume");
        db::ensure_mcp_server(&pool, "exa", "https://mcp.example.com/mcp")
            .await
            .expect("add mcp server");
        let env = prompt_environment(&pool).await;
        assert_eq!(env.date, chrono::Utc::now().date_naive());
        assert_eq!(env.model, anthropic_model());
        assert_eq!(
            env.volumes,
            vec![("cargo-cache".to_string(), "/home/sandbox/.cargo".to_string())]
        );
        assert_eq!(env.mcp_servers, vec!["exa".to_string()]);
    }

    /// Every turn's request carries the system prompt built from that
    /// turn's environment.
    #[sqlx::test]
    async fn test_run_turn_sends_the_system_prompt(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");
        db::create_sandbox_volume(&pool, "cargo-cache", "/home/sandbox/.cargo")
            .await
            .expect("create volume");
        let requests = start_recording_mock_upstream(vec![text_reply_body("Hi!")]).await;

        run_turn(&pool, conversation.id, hello(), None)
            .await
            .expect("run_turn should succeed");

        let requests = requests.lock().expect("the request log");
        assert_eq!(requests.len(), 1);
        let expected = system_prompt(&prompt_environment(&pool).await);
        assert_eq!(requests[0]["system"].as_str(), Some(expected.as_str()));
        assert!(expected.contains("cargo-cache mounted at /home/sandbox/.cargo"));
    }

    /// Compaction's summarization call keeps its own prompt; only the real
    /// turn after it gets the agent's system prompt.
    #[sqlx::test]
    async fn test_compaction_keeps_its_own_system_prompt(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");
        db::create_message(
            &pool,
            conversation.id,
            "user",
            &[anthropic::ContentBlock::Text {
                text: "earlier message".to_string(),
            }],
        )
        .await
        .expect("seed earlier message");
        // Usage past the ceiling, so the next turn compacts first (see
        // `test_run_turn_compacts_before_sending_when_usage_is_near_the_ceiling`).
        db::upsert_conversation_usage(
            &pool,
            conversation.id,
            &anthropic::TokenUsage {
                input_tokens: 190_000,
                output_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
        )
        .await
        .expect("seed usage");
        let requests = start_recording_mock_upstream(vec![
            text_reply_body("Summary: nothing live."),
            text_reply_body("Hi again"),
        ])
        .await;

        run_turn(&pool, conversation.id, hello(), None)
            .await
            .expect("run_turn should succeed");

        let requests = requests.lock().expect("the request log");
        assert_eq!(requests.len(), 2, "a compaction call, then the real turn");
        assert_eq!(requests[0]["system"].as_str(), Some(COMPACTION_SYSTEM_PROMPT));
        let expected = system_prompt(&prompt_environment(&pool).await);
        assert_eq!(requests[1]["system"].as_str(), Some(expected.as_str()));
    }

    /// The detail view shows exactly the system prompt a turn sends.
    #[sqlx::test]
    async fn test_context_detail_shows_the_system_prompt_a_turn_sends(pool: PgPool) {
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");
        let detail = context_detail(&pool, conversation.id)
            .await
            .expect("context detail");
        let expected = system_prompt(&prompt_environment(&pool).await);
        assert_eq!(detail.system.as_deref(), Some(expected.as_str()));
    }

    /// Every tool the base prompt names in backticks must exist, so
    /// renaming or removing a tool fails here until the prompt is updated.
    /// A backticked word counts as a tool name when it's lowercase letters
    /// and underscores; the few that aren't tools are listed.
    #[test]
    fn test_system_prompt_only_names_real_tools() {
        const NOT_TOOLS: &[&str] = &["sandbox", "sudo", "web_search"];
        let tools: Vec<String> = anthropic::tools::native_tool_definitions()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        let named: Vec<&str> = BASE_SYSTEM_PROMPT
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|word| !word.is_empty() && word.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
            .filter(|word| !NOT_TOOLS.contains(word))
            .collect();
        assert!(named.len() > 10, "expected the prompt to name its tools, found {named:?}");
        let unknown: Vec<&&str> = named.iter().filter(|word| !tools.iter().any(|t| t == **word)).collect();
        assert!(unknown.is_empty(), "the system prompt names tools that don't exist: {unknown:?}");
    }

    /// A mock Anthropic upstream that accepts requests and never answers,
    /// for a turn that stays in flight until stopped. Same locking rule as
    /// `start_mock_upstream`.
    async fn start_hanging_mock_upstream() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(|| async {
                std::future::pending::<()>().await;
                ""
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        unsafe {
            std::env::set_var("ANTHROPIC_BASE_URL", format!("http://{addr}"));
            std::env::set_var("ANTHROPIC_API_KEY", "test-key");
        }
    }

    /// Stopping ends an in-flight turn at once, frees the turn lock, and
    /// keeps the user's message.
    #[sqlx::test]
    async fn test_stop_turn_ends_a_turn_in_flight(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation_with_id(&pool, 9100000001)
            .await
            .expect("create conversation");
        start_hanging_mock_upstream().await;

        let turn = tokio::spawn({
            let pool = pool.clone();
            async move { run_turn(&pool, conversation.id, hello(), None).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        stop_turn_now(conversation.id);

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), turn)
            .await
            .expect("the turn should end within a second of stopping")
            .expect("join");
        let error = result.expect_err("a stopped turn ends with an error").to_string();
        assert!(error.contains(TURN_STOPPED), "got: {error}");
        assert!(
            conversation_lock(conversation.id).try_lock().is_ok(),
            "a stopped turn should release the turn lock"
        );
        let saved = db::list_messages(&pool, conversation.id).await.expect("list");
        assert_eq!(saved.len(), 1, "the user's message is kept");

        // A turn started after the stop isn't affected by it.
        let later = start_recording_mock_upstream(vec![text_reply_body("Hi!")]).await;
        run_turn(&pool, conversation.id, hello(), None)
            .await
            .expect("a later turn runs normally");
        assert_eq!(later.lock().expect("log").len(), 1);
    }

    /// Every tab can tell a turn is running, including one a background
    /// notice started, and when it ends.
    #[sqlx::test]
    async fn test_turn_state_is_published_while_a_turn_runs(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation_with_id(&pool, 9100000002)
            .await
            .expect("create conversation");
        start_hanging_mock_upstream().await;
        let mut rx = events::subscribe(conversation.id);
        assert!(!turn_running(conversation.id));

        let turn = tokio::spawn({
            let pool = pool.clone();
            async move { run_turn(&pool, conversation.id, hello(), None).await }
        });
        let started = next_turn_state(&mut rx).await;
        assert_eq!(started, Some(true), "a turn starting should say so");
        assert!(turn_running(conversation.id));

        stop_turn_now(conversation.id);
        let _ = turn.await;
        let ended = next_turn_state(&mut rx).await;
        assert_eq!(ended, Some(false), "a turn ending (here, stopped) should say so");
        assert!(!turn_running(conversation.id));
    }

    async fn next_turn_state(
        rx: &mut tokio::sync::broadcast::Receiver<events::ConversationEvent>,
    ) -> Option<bool> {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match rx.recv().await {
                    Ok(events::ConversationEvent::TurnState { running }) => return Some(running),
                    Ok(_) => continue,
                    Err(_) => return None,
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    #[test]
    fn test_stop_turn_with_nothing_running_does_nothing() {
        stop_turn_now(9_000_000_012);
        let receiver = stop_receiver(9_000_000_012);
        assert!(!receiver.has_changed().expect("open"), "an earlier stop must not affect a new turn");
    }

    /// After a stop, a finished command doesn't wake the model; the user's
    /// next message does, and its turn includes the command's notice.
    #[sqlx::test]
    async fn test_a_stopped_conversation_waits_for_the_user_before_waking(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation_with_id(&pool, 9100000003)
            .await
            .expect("create conversation");
        let requests = start_recording_mock_upstream(vec![text_reply_body("Hi!")]).await;

        stop_turn_now(conversation.id);
        assert!(is_paused(conversation.id), "a stop pauses the conversation");
        unnotified_finished_command(&pool, conversation.id, "cmd-after-stop").await;
        wake_conversation(&pool, conversation.id)
            .await
            .expect("a paused wake does nothing, successfully");
        assert!(
            requests.lock().expect("log").is_empty(),
            "a paused conversation shouldn't call the model"
        );

        resume_turns(conversation.id);
        assert!(!is_paused(conversation.id));
        run_turn(&pool, conversation.id, hello(), None)
            .await
            .expect("the user's next turn runs");
        let requests = requests.lock().expect("log");
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]["messages"].to_string().contains("cmd-after-stop"),
            "the next turn should include the command's notice"
        );
    }

    /// A notice waits for a running turn to end before it's saved, and
    /// tells a watching tab once it is.
    #[sqlx::test]
    async fn test_save_notice_between_turns_waits_for_the_turn_lock(pool: PgPool) {
        let conversation = db::create_conversation_with_id(&pool, 9100000004)
            .await
            .expect("create conversation");
        let lock = conversation_lock(conversation.id);
        let turn = lock.lock().await;
        let mut events = events::subscribe(conversation.id);

        let saving = tokio::spawn({
            let pool = pool.clone();
            async move {
                save_notice_between_turns(&pool, conversation.id, "pod stopped".to_string()).await
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            db::list_messages(&pool, conversation.id).await.expect("list").is_empty(),
            "the notice was saved while a turn held the lock"
        );

        drop(turn);
        let saved = saving.await.expect("join").expect("save the notice");
        assert_eq!(saved.role, "user");
        assert_eq!(
            saved.blocks().expect("blocks"),
            vec![anthropic::ContentBlock::Text { text: "pod stopped".to_string() }]
        );
        let appended = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match events.recv().await {
                    Ok(events::ConversationEvent::MessagesAppended { messages }) => return messages,
                    Ok(_) => continue,
                    Err(e) => panic!("event channel: {e}"),
                }
            }
        })
        .await
        .expect("a MessagesAppended event");
        assert_eq!(appended.len(), 1);
    }

    fn hello() -> anthropic::AnthropicMessage {
        anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "hello".to_string(),
            }],
        }
    }

    #[sqlx::test]
    async fn test_run_turn_for_a_missing_conversation_says_so(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        start_mock_upstream(vec![String::new()]).await;
        let error = run_turn(&pool, 987_654_321, hello(), None)
            .await
            .expect_err("a turn for a conversation that doesn't exist should fail")
            .to_string();
        assert!(error.contains("conversation not found"), "got: {error}");
        assert!(!error.contains("foreign key"), "a raw database error leaked: {error}");
    }

    /// With no model credentials the turn can't run, but what the user
    /// typed is still theirs: it shouldn't vanish on the next reload.
    #[sqlx::test]
    async fn test_run_turn_without_credentials_still_saves_the_message(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");
        // SAFETY: the lock above serializes every test that touches these.
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("ANTHROPIC_AUTH_TOKEN");
        }
        let result = run_turn(&pool, conversation.id, hello(), None).await;
        assert!(result.is_err(), "no credentials should fail the turn");
        let saved = db::list_messages(&pool, conversation.id)
            .await
            .expect("list messages");
        assert_eq!(saved.len(), 1, "the user's message should have been saved");
        assert_eq!(saved[0].role, "user");
    }

    #[sqlx::test]
    async fn test_run_turn_persists_user_and_assistant_messages_for_text_only_reply(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        let body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi!"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![body]).await;

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "hello".to_string(),
            }],
        };

        let messages = run_turn(&pool, conversation.id, new_message, None)
            .await
            .expect("run_turn should succeed");

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(
            messages[0].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::Text {
                text: "hello".to_string()
            }]
        );
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(
            messages[1].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::Text {
                text: "Hi!".to_string()
            }]
        );
    }

    /// Regression test for conversation 43's real "500 error parsing tool
    /// call" incident: with thinking on (the default) and pointed at a
    /// mock upstream that fails the *first* request with Ollama's exact
    /// error shape, `run_turn` should retry without thinking and still
    /// complete — not surface the 500 to the caller.
    #[sqlx::test]
    async fn test_run_turn_retries_without_thinking_after_ollama_tool_call_corruption(
        pool: PgPool,
    ) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        let success_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"pong"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream_failing_n_times(1, Some(success_body)).await;

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "ping".to_string(),
            }],
        };

        let messages = run_turn(&pool, conversation.id, new_message, None)
            .await
            .expect("run_turn should recover from the failed first attempt and succeed");

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(
            messages[1].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::Text {
                text: "pong".to_string()
            }],
            "the retried (thinking-free) attempt's reply should be what actually got persisted"
        );
    }

    /// Dropping `thinking` doesn't help every case (a local model can flub
    /// a tool call's JSON on its own — see `is_ollama_thinking_tool_call_corruption`'s
    /// doc comment) — this fails *twice*, past the thinking-drop, and
    /// relies on `TOOL_CALL_PARSE_RETRIES` allowing one further plain
    /// regeneration to still recover.
    #[sqlx::test]
    async fn test_run_turn_recovers_after_two_ollama_tool_call_failures(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        let success_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"third time's the charm"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream_failing_n_times(2, Some(success_body)).await;

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "ping".to_string(),
            }],
        };

        let messages = run_turn(&pool, conversation.id, new_message, None)
            .await
            .expect(
                "run_turn should recover after exhausting the thinking-drop and one plain retry",
            );

        assert_eq!(
            messages[1].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::Text {
                text: "third time's the charm".to_string()
            }]
        );
    }

    /// Once `TOOL_CALL_PARSE_RETRIES` is exhausted, `run_turn` gives up and
    /// surfaces the error rather than retrying forever against a call the
    /// model is reliably bad at.
    #[sqlx::test]
    async fn test_run_turn_gives_up_after_exhausting_tool_call_parse_retries(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        // `fail_count` is irrelevant when `success_body` is `None` — every
        // request fails regardless (see the helper's doc comment).
        start_mock_upstream_failing_n_times(0, None).await;

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "ping".to_string(),
            }],
        };

        let err = run_turn(&pool, conversation.id, new_message, None)
            .await
            .expect_err("should give up and surface the error once retries are exhausted");
        assert!(
            err.to_string().contains("error parsing tool call"),
            "got {err}"
        );
    }

    /// A two-step reply: "Adding." and a call to `add`, then (after the
    /// tool result) "Sum is 5". For the mock upstream, in order.
    fn text_tool_then_text_bodies() -> Vec<String> {
        let first = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Adding."}}"#,
            ),
            ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01","name":"add","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\":2,\"b\":3}"}}"#,
            ),
            ("content_block_stop", r#"{"type":"content_block_stop","index":1}"#),
            ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        vec![first, text_reply_body("Sum is 5")]
    }

    /// Collects `rx`'s events until nothing arrives for half a second.
    async fn drain_events(
        rx: &mut tokio::sync::broadcast::Receiver<events::ConversationEvent>,
    ) -> Vec<events::ConversationEvent> {
        let mut seen = Vec::new();
        while let Ok(Ok(event)) =
            tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await
        {
            seen.push(event);
        }
        seen
    }

    /// Every tab can show a reply as it streams, one bubble per model
    /// call: a reset when each call starts, then its text.
    #[sqlx::test]
    async fn test_a_turn_streams_its_reply_to_every_tab(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation_with_id(&pool, 9_100_000_007)
            .await
            .expect("create conversation");
        start_mock_upstream(text_tool_then_text_bodies()).await;
        let mut rx = events::subscribe(conversation.id);

        run_turn(&pool, conversation.id, hello(), None)
            .await
            .expect("run_turn should succeed");

        let streamed: Vec<String> = drain_events(&mut rx)
            .await
            .into_iter()
            .filter_map(|event| match event {
                events::ConversationEvent::ReplyReset {} => Some("<reset>".to_string()),
                events::ConversationEvent::ReplyDelta { text } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(streamed, vec!["<reset>", "Adding.", "<reset>", "Sum is 5"]);
        assert_eq!(reply_in_progress(conversation.id), None, "nothing in progress once the turn ends");
    }

    /// A mock upstream that streams `text` and then never finishes. Same
    /// locking rule as `start_mock_upstream`.
    async fn start_partial_then_hanging_mock_upstream(text: &'static str) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move || async move {
                let start = format!(
                    "event: message_start\ndata: {{\"type\":\"message_start\"}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"
                );
                let body = futures_util::StreamExt::chain(
                    futures_util::stream::once(async move { Ok::<_, std::io::Error>(start) }),
                    futures_util::stream::pending(),
                );
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    axum::body::Body::from_stream(body),
                )
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        unsafe {
            std::env::set_var("ANTHROPIC_BASE_URL", format!("http://{addr}"));
            std::env::set_var("ANTHROPIC_API_KEY", "test-key");
        }
    }

    /// A tab that connects mid-reply can show the text so far.
    #[sqlx::test]
    async fn test_the_reply_so_far_is_available_mid_turn(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation_with_id(&pool, 9_100_000_008)
            .await
            .expect("create conversation");
        start_partial_then_hanging_mock_upstream("Partial answer").await;

        let turn = tokio::spawn({
            let pool = pool.clone();
            async move { run_turn(&pool, conversation.id, hello(), None).await }
        });
        let seen = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(text) = reply_in_progress(conversation.id) {
                    return text;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the partial reply should be available mid-turn");
        assert_eq!(seen, "Partial answer");

        stop_turn_now(conversation.id);
        let _ = turn.await;
        assert_eq!(reply_in_progress(conversation.id), None, "a stopped turn leaves nothing in progress");
    }

    /// Sending returns at once; the turn carries on and every tab hears it.
    #[sqlx::test]
    async fn test_start_turn_returns_before_the_turn_finishes(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation_with_id(&pool, 9_100_000_010)
            .await
            .expect("create conversation");
        start_hanging_mock_upstream().await;
        let mut rx = events::subscribe(conversation.id);

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            start_turn(pool.clone(), conversation.id, "hello there".to_string()),
        )
        .await
        .expect("sending shouldn't wait for the model")
        .expect("sending should succeed");

        let user_message = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(events::ConversationEvent::MessagesAppended { messages }) = rx.recv().await {
                    return messages;
                }
            }
        })
        .await
        .expect("the user's message should be published");
        assert_eq!(user_message.len(), 1);
        assert!(user_message[0].content.contains("hello there"));
        assert!(turn_running(conversation.id), "the turn should still be running");
        stop_turn_now(conversation.id);
    }

    #[sqlx::test]
    async fn test_start_turn_refuses_a_missing_conversation(pool: PgPool) {
        let error = start_turn(pool, 9_100_000_011, "hi".to_string())
            .await
            .expect_err("a conversation that doesn't exist")
            .to_string();
        assert!(error.contains("conversation not found"), "got: {error}");
    }

    /// A sent turn that fails, or is stopped, tells every tab why.
    #[sqlx::test]
    async fn test_a_sent_turn_publishes_its_failure(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let failing = db::create_conversation_with_id(&pool, 9_100_000_012)
            .await
            .expect("create conversation");
        start_mock_upstream_failing_n_times(0, None).await;
        let mut rx = events::subscribe(failing.id);
        start_turn(pool.clone(), failing.id, "hi".to_string()).await.expect("send");
        let error = next_turn_error(&mut rx).await.expect("a TurnError");
        assert!(error.contains("error parsing tool call"), "got: {error}");

        let stopped = db::create_conversation_with_id(&pool, 9_100_000_013)
            .await
            .expect("create conversation");
        start_hanging_mock_upstream().await;
        let mut rx = events::subscribe(stopped.id);
        start_turn(pool.clone(), stopped.id, "hi".to_string()).await.expect("send");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        stop_turn_now(stopped.id);
        assert_eq!(next_turn_error(&mut rx).await.as_deref(), Some(TURN_STOPPED));
    }

    async fn next_turn_error(
        rx: &mut tokio::sync::broadcast::Receiver<events::ConversationEvent>,
    ) -> Option<String> {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match rx.recv().await {
                    Ok(events::ConversationEvent::TurnError { message }) => return Some(message),
                    Ok(_) => continue,
                    Err(_) => return None,
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// Stopping a turn a finished command started isn't a failed
    /// notification.
    #[sqlx::test]
    async fn test_stopping_a_woken_turn_is_not_reported_as_a_failure(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation_with_id(&pool, 9_100_000_014)
            .await
            .expect("create conversation");
        unnotified_finished_command(&pool, conversation.id, "cmd-woken").await;
        start_hanging_mock_upstream().await;
        let mut rx = events::subscribe(conversation.id);

        let wake = tokio::spawn({
            let pool = pool.clone();
            async move { wake_conversation(&pool, conversation.id).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        stop_turn_now(conversation.id);
        let _ = wake.await;

        let reported = drain_events(&mut rx)
            .await
            .into_iter()
            .any(|e| matches!(e, events::ConversationEvent::NotificationDeliveryFailed { .. }));
        assert!(!reported, "a stop isn't a failed notification");
    }

    /// Every tab sees each message as it's saved: the user's own first,
    /// then each step of a multi-step reply, not one batch at the end.
    #[sqlx::test]
    async fn test_a_turn_publishes_each_message_as_it_is_saved(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation_with_id(&pool, 9_100_000_006)
            .await
            .expect("create conversation");
        start_mock_upstream(text_tool_then_text_bodies()).await;
        let mut rx = events::subscribe(conversation.id);

        run_turn(&pool, conversation.id, hello(), None)
            .await
            .expect("run_turn should succeed");

        let batches: Vec<Vec<String>> = drain_events(&mut rx)
            .await
            .into_iter()
            .filter_map(|event| match event {
                events::ConversationEvent::MessagesAppended { messages } => {
                    Some(messages.into_iter().map(|m| m.role).collect())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            batches,
            vec![
                vec!["user".to_string()],
                vec!["assistant".to_string()],
                vec!["user".to_string()],
                vec!["assistant".to_string()],
            ],
            "one event per saved message, in order"
        );
    }

    #[sqlx::test]
    async fn test_run_turn_executes_tool_and_persists_full_round_trip(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        let tool_use_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"add","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":2,\"b\":3}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        let final_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Sum is 5"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![tool_use_body, final_body]).await;

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "please add 2 and 3".to_string(),
            }],
        };

        let messages = run_turn(&pool, conversation.id, new_message, None)
            .await
            .expect("run_turn should succeed");

        assert_eq!(
            messages.len(),
            4,
            "expected user, tool_use, tool_result, final assistant"
        );
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(
            messages[1].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::ToolUse {
                id: "toolu_01".to_string(),
                name: "add".to_string(),
                input: serde_json::json!({"a": 2, "b": 3}),
            }]
        );
        assert_eq!(messages[2].role, "user");
        assert_eq!(
            messages[2].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::ToolResult {
                tool_use_id: "toolu_01".to_string(),
                content: "5".to_string(),
                is_error: None,
            }]
        );
        assert_eq!(messages[3].role, "assistant");
        assert_eq!(
            messages[3].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::Text {
                text: "Sum is 5".to_string()
            }]
        );
    }

    #[sqlx::test]
    async fn test_describe_live_state_lists_real_pods_terminals_and_tasks(pool: PgPool) {
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        // Deliberately real rows/a real registry entry, not hand-built
        // strings — this is the exact data `compact_conversation`'s
        // summarization prompt hands the model, so the format it actually
        // produces from real fixtures is what matters, not an assumption
        // about it.
        let pod = db::create_sandbox_pod(&pool, conversation.id)
            .await
            .expect("create pod");
        let terminal = db::create_sandbox_terminal(&pool, pod.id)
            .await
            .expect("create terminal");
        anthropic::tools::execute(
            &pool,
            conversation.id,
            "toolu_describe_live_state_test",
            "run_async",
            &serde_json::json!({"tool": "add", "input": {"a": 1, "b": 2}}),
        )
        .await
        .expect("seed background task");

        let description = describe_live_state(&pool, conversation.id).await;

        assert!(
            description.contains(&format!("pod_id {}", pod.id)),
            "expected the real pod id in: {description}"
        );
        assert!(
            description.contains(&format!("terminal_id {} in pod_id {}", terminal.id, pod.id)),
            "expected the real terminal id (and its pod) in: {description}"
        );
        assert!(
            description.contains("toolu_describe_live_state_test"),
            "expected the real task id in: {description}"
        );
        assert!(
            description.contains("add"),
            "expected the task's tool name in: {description}"
        );
    }

    #[sqlx::test]
    async fn test_describe_live_state_lists_the_current_todo_list(pool: PgPool) {
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");
        db::set_conversation_todos(
            &pool,
            conversation.id,
            &[
                anthropic::tools::TodoItem {
                    content: "write the plan".to_string(),
                    status: anthropic::tools::TodoStatus::Completed,
                },
                anthropic::tools::TodoItem {
                    content: "implement".to_string(),
                    status: anthropic::tools::TodoStatus::InProgress,
                },
            ],
        )
        .await
        .expect("seed todos");

        let description = describe_live_state(&pool, conversation.id).await;

        assert!(
            description.contains("write the plan") && description.contains("completed"),
            "expected the first todo and its status in: {description}"
        );
        assert!(
            description.contains("implement") && description.contains("in_progress"),
            "expected the second todo and its status in: {description}"
        );
    }

    #[sqlx::test]
    async fn test_run_turn_compacts_before_sending_when_usage_is_near_the_ceiling(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        // A prior turn's persisted message plus usage close enough to the
        // reserved ceiling (200_000 - 16_384 = 183_616 for the default
        // "claude-opus-4-8" model — see `context_window_for`/`MAX_TOKENS`)
        // that the very next turn must compact before sending, regardless
        // of how small the new message's own estimate is.
        db::create_message(
            &pool,
            conversation.id,
            "user",
            &[anthropic::ContentBlock::Text {
                text: "earlier message".to_string(),
            }],
        )
        .await
        .expect("seed earlier message");
        db::upsert_conversation_usage(
            &pool,
            conversation.id,
            &anthropic::TokenUsage {
                input_tokens: 190_000,
                output_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
        )
        .await
        .expect("seed usage");

        // First request the mock upstream sees is the compaction's own
        // summarization call; the second is the real turn, sent afterward
        // using the now-compacted history.
        let summary_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Summary: discussed earlier topics. Nothing currently live."}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        let real_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi again"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![summary_body, real_body]).await;

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "new question".to_string(),
            }],
        };

        let messages = run_turn(&pool, conversation.id, new_message, None)
            .await
            .expect("run_turn should succeed");

        let all_messages = db::list_messages(&pool, conversation.id)
            .await
            .expect("list messages");
        let compaction_summary = all_messages.iter().find_map(|m| {
            m.blocks().ok()?.into_iter().find_map(|b| match b {
                anthropic::ContentBlock::CompactionSummary {
                    summary,
                    covers_through_message_id,
                } => Some((summary, covers_through_message_id)),
                _ => None,
            })
        });
        assert!(
            compaction_summary.is_some(),
            "expected a CompactionSummary message to have been persisted, got: {all_messages:?}"
        );
        assert!(
            compaction_summary
                .unwrap()
                .0
                .contains("discussed earlier topics"),
            "expected the real summarization response's text to be what got persisted"
        );

        // Nothing already stored was rewritten or deleted — the original
        // seeded message and the new question are both still there in
        // full, untouched.
        assert!(
            all_messages
                .iter()
                .any(|m| m
                    .blocks()
                    .unwrap_or_default()
                    .contains(&anthropic::ContentBlock::Text {
                        text: "earlier message".to_string()
                    }))
        );
        assert!(
            all_messages
                .iter()
                .any(|m| m
                    .blocks()
                    .unwrap_or_default()
                    .contains(&anthropic::ContentBlock::Text {
                        text: "new question".to_string()
                    }))
        );

        // The real turn still completed successfully, replying to the
        // post-compaction continuation prompt.
        let last = messages.last().expect("at least one message");
        assert_eq!(last.role, "assistant");
        assert_eq!(
            last.blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::Text {
                text: "Hi again".to_string()
            }]
        );
    }

    #[sqlx::test]
    async fn test_run_turn_errors_when_max_turns_exceeded(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        // Always responds with a tool_use turn calling `add` (a fast, valid
        // call), so the loop never reaches a final reply and must give up
        // after MAX_TURNS rather than looping forever.
        let tool_use_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"add","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":1,\"b\":1}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![tool_use_body]).await;

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "loop forever".to_string(),
            }],
        };

        // Goes through run_turn_bounded directly with a small bound rather
        // than run_turn (which would replay the mock upstream the real
        // MAX_TURNS — 10,000 — times just to prove the same "give up and
        // error" behavior).
        let result = run_turn_bounded(&pool, conversation.id, Some(new_message), None, 3).await;
        assert!(result.is_err(), "expected an error, got {result:?}");
    }

    #[test]
    fn test_conversation_lock_is_shared_per_conversation_id_only() {
        let a1 = conversation_lock(9001);
        let a2 = conversation_lock(9001);
        let b = conversation_lock(9002);
        assert!(
            Arc::ptr_eq(&a1, &a2),
            "same conversation id should share one lock"
        );
        assert!(
            !Arc::ptr_eq(&a1, &b),
            "different conversation ids should get different locks"
        );
    }

    #[sqlx::test]
    async fn test_run_turn_serializes_concurrent_calls_for_the_same_conversation(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        let body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![body]).await;

        let conversation_id = conversation.id;
        let pool_a = pool.clone();
        let pool_b = pool.clone();

        let task_a = tokio::spawn(async move {
            let message = anthropic::AnthropicMessage {
                role: "user".to_string(),
                content: vec![anthropic::ContentBlock::Text {
                    text: "first".to_string(),
                }],
            };
            run_turn(&pool_a, conversation_id, message, None).await
        });
        let task_b = tokio::spawn(async move {
            let message = anthropic::AnthropicMessage {
                role: "user".to_string(),
                content: vec![anthropic::ContentBlock::Text {
                    text: "second".to_string(),
                }],
            };
            run_turn(&pool_b, conversation_id, message, None).await
        });

        let (result_a, result_b) = tokio::join!(task_a, task_b);
        result_a
            .expect("task a should not panic")
            .expect("run_turn a should succeed");
        result_b
            .expect("task b should not panic")
            .expect("run_turn b should succeed");

        let all = db::list_messages(&pool, conversation_id)
            .await
            .expect("list messages");
        assert_eq!(all.len(), 4);
        let roles: Vec<&str> = all.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "user", "assistant"],
            "the per-conversation lock should serialize the two calls into two complete \
             (user, assistant) pairs, never interleaved"
        );
    }

    /// Regression test for a deadlock: `cancel_task` runs synchronously
    /// inside `run_turn`'s own tool-dispatch loop (same as `add`/`count`/
    /// any other tool), which already holds `conversation_id`'s lock for
    /// its entire duration. `cancel_task` also pushes a cancellation
    /// notification via `chat::run_turn` — if that push were awaited
    /// in-line rather than detached (`tokio::spawn`), it would try to
    /// re-acquire the same non-reentrant lock the outer call is still
    /// holding and hang forever. Wrapped in a timeout so a regression
    /// fails loudly instead of hanging the test suite.
    #[sqlx::test]
    async fn test_run_turn_does_not_deadlock_when_model_calls_cancel_task(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        // Seed a running task directly (bypassing the model) for cancel_task
        // to act on.
        let start_result = anthropic::tools::execute(
            &pool,
            conversation.id,
            "toolu_seed_task",
            "run_async",
            &serde_json::json!({"tool": "count", "input": {"target": 5, "interval_seconds": 5}}),
        )
        .await
        .expect("seeding the background task should succeed");
        assert!(start_result.contains("toolu_seed_task"));

        let cancel_turn_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_cancel","name":"cancel_task","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"task_id\":\"toolu_seed_task\"}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        let final_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"cancelled it"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![cancel_turn_body, final_body]).await;

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "cancel that task".to_string(),
            }],
        };

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_turn(&pool, conversation.id, new_message, None),
        )
        .await
        .expect("run_turn should complete well within the timeout, not deadlock")
        .expect("run_turn should succeed");

        assert_eq!(
            result.len(),
            4,
            "expected user, tool_use(cancel_task), tool_result, final assistant"
        );
        assert_eq!(
            result[2].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::ToolResult {
                tool_use_id: "toolu_cancel".to_string(),
                content: "task toolu_seed_task cancelled".to_string(),
                is_error: None,
            }]
        );
    }

    /// Regression test: `run_async` and the task-management suite were
    /// fully implemented and unit-tested in `anthropic::tools` before
    /// anyone noticed `tool_definitions()` never listed them — the model
    /// had no way to know they existed until a live test asked it to use
    /// `run_async` and it correctly said no such tool was available. This
    /// pins the two lists together so a newly dispatchable tool can't be
    /// implemented without also being offered to the model.
    ///
    /// Scoped to `native_tool_definitions()` deliberately — this test is
    /// about smelt's own static dispatch names, not MCP servers (which are
    /// dynamic/external and have no fixed name list to pin against).
    #[test]
    fn test_tool_definitions_covers_every_dispatchable_tool_name() {
        let defined: std::collections::BTreeSet<String> =
            anthropic::tools::native_tool_definitions()
                .into_iter()
                .map(|t| t.name)
                .collect();
        let dispatchable: std::collections::BTreeSet<&str> = [
            "add",
            "count",
            "echo",
            "run_async",
            "list_tasks",
            "task_status",
            "task_stdout",
            "task_stderr",
            "task_result",
            "wait_task",
            "cancel_task",
            "write_task_stdin",
            "create_pod",
            "terminate_pod",
            "list_pods",
            "create_terminal",
            "terminate_terminal",
            "list_terminals",
            "run_terminal_command",
            "send_signal",
            "terminal_command_status",
            "read_terminal_output",
            "list_commands",
        ]
        .into_iter()
        .collect();

        let missing: Vec<_> = dispatchable
            .iter()
            .filter(|name| !defined.contains(**name))
            .collect();
        assert!(
            missing.is_empty(),
            "native_tool_definitions() is missing: {missing:?}"
        );
    }
}
