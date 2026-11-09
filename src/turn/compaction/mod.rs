//! Compacting a conversation that nears its model's context window.

use super::*;

mod budgets;
mod continuation;
pub(super) use budgets::*;
pub(super) use continuation::*;
/// Whether it's safe to end a compaction's covered range right after a
/// message with this content — i.e. it contains no `tool_use` block whose
/// paired `tool_result` (always the very next persisted message, in this
/// codebase's per-turn persistence model) hasn't been included too.
/// Anthropic rejects a request with an orphaned half outright, so this is
/// a hard constraint on where `covers_through_message_id` may point, never
/// best-effort. See SME-18.
#[cfg(feature = "server")]
pub(super) fn is_safe_compaction_boundary(blocks: &[anthropic::ContentBlock]) -> bool {
    !blocks
        .iter()
        .any(|block| matches!(block, anthropic::ContentBlock::ToolUse { .. }))
}

/// A fixed, structural placeholder — never shown as if the user actually
/// typed it, just what makes the synthetic post-compaction exchange below
/// start validly with `user` (Anthropic's own requirement, not negotiable).
#[cfg(feature = "server")]
pub(super) const COMPACTION_PLACEHOLDER_PROMPT: &str = "Summarize the conversation so far.";

/// The synthetic message that actually gets a real response after
/// compaction — the summary already carries forward whatever the model
/// needs (including the gist of whatever large content triggered
/// compaction in the first place, since the summarization call sees
/// everything up through it), so this is a plain continuation nudge, not
/// a replay of anything. Since SME-393 the nudge names what the summary
/// is — a narration to verify, not a record to act on: on 2294639560 a
/// seat read it as a record, "confirmed" imaginary HEADs against it, and
/// destroyed its real uncommitted work restoring them.
#[cfg(feature = "server")]
pub(super) const COMPACTION_CONTINUATION_PROMPT: &str = "Continue based on the summary above. \
The summary is a narration of the conversation, not a record of state: before acting on any \
state it claims (commits, pushes, PRs, CI runs, files), verify that state with a tool call — \
always before any command that would overwrite, discard or reset work.";
/// The system prompt for the dedicated summarization call `compact_conversation`
/// makes — no tools attached, so this is the model's only instruction.
/// Explicitly demands every live id be preserved verbatim rather than
/// trusting the model to notice one buried in a long transcript
/// unprompted — see `describe_live_state` below, which hands them over
/// directly rather than leaving them to be spotted. And since SME-393 it
/// binds the summary to evidence: Done only where a tool result completes
/// the action, everything else Planned — a seat destroyed its real
/// uncommitted work acting on commits a later model turn had narrated but
/// no tool ever made.
#[cfg(feature = "server")]
pub(super) const COMPACTION_SYSTEM_PROMPT: &str = "You are compacting an AI coding agent's \
conversation history to free up context window space. Write a concise summary \
of the conversation so far: the user's original goal, key decisions made, the \
current state of any files or code changed, and any unresolved next steps. \
Separate what tool results show from everything else: put under Done only \
actions a tool result in the transcript completes — a commit SHA, a pushed \
branch, a PR number, a test run — each naming the tool result that evidences \
it; everything started, intended or mid-way goes under Planned, including \
work whose file edits are visible but whose completing tool result is not \
shown. An action is never done because earlier text — including the \
assistant's own — said it happened: words are not evidence, tool results \
are. After an [earlier conversation omitted] cut, say the state there is \
unknown. You MUST explicitly mention, by its exact id, every sandbox pod and \
terminal listed below as currently live — never omit or paraphrase an \
id, since later tool calls still need a working reference to it. Write the \
summary as prose, or with its parts labelled Done and Planned where that \
keeps the record straight.";

/// A plain-text listing of every currently-live pod/terminal for
/// `conversation_id`, handed to the summarization call so it can be told
/// directly what must survive — see `COMPACTION_SYSTEM_PROMPT` and
/// SME-18's "Interaction with live/
/// in-flight state." Best-effort: a lookup failure just omits that
/// category rather than failing the whole compaction over it.
#[cfg(feature = "server")]
pub(super) async fn describe_live_state(pool: &PgPool, conversation_id: i64) -> String {
    // Deliberately the plain `db::` row queries, not `sandbox::list_pods`/
    // `list_terminals` — those also reach the cluster to check live
    // connection status, which a summarization prompt doesn't need: just
    // which ids exist and aren't terminated.
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

/// The most a summary may write (floor of the derived budget since
/// SME-443 — `summary_max_tokens` sends HTTP kinds more when their window
/// leaves room; Claude Code keeps this constant, `claude_cli::once` adding
/// its `THINKING_ROOM` on top).
#[cfg(feature = "server")]
pub(super) const SUMMARY_MAX_TOKENS: u32 = 2048;

/// The ceiling of a room-derived summary budget (SME-443): 4× the floor,
/// ~3% of a 262k window, and below current models' own output caps —
/// which `summary_max_tokens` clamps to anyway, Anthropic 400s above them.
/// Decided (Driver) Q1, 2026-11-09; the ticket's cap-report measurement can
/// revise it before merge.
#[cfg(feature = "server")]
pub(super) const SUMMARY_MAX_TOKENS_CEILING: u32 = 8_192;

/// The summary call's `max_tokens` (SME-443): the model's window less the
/// built request's size at 3 chars/token — `compaction_transcript_budget`'s
/// own conservative rate, so transcript size and budget stay consistent —
/// less `COMPACTION_SAFETY_BUFFER`, floored at today's 2,048 and held to
/// `SUMMARY_MAX_TOKENS_CEILING`. `request_chars` is the transcript prompt
/// the request carries — the system prompt and JSON framing ride on top,
/// inside the 4,096 buffer's margin — so it grows for the continuation
/// request. The 2,048 floor wins over a stingy room: a request the provider
/// then refuses as too big is exactly what `compact`'s existing overflow
/// arm answers — learn the window, retry with half the transcript. The
/// model's own *reported* cap clamps the result whatever the user's "Max
/// reply tokens" made the applied cap: over its own cap Anthropic 400s
/// "maximum allowed", a refusal the overflow arm deliberately does not
/// answer (review 1). The user's cap itself does not apply — a summary is
/// not a reply (SME-111).
#[cfg(feature = "server")]
pub(super) fn summary_max_tokens(turn_model: &crate::providers::TurnModel, request_chars: usize) -> u32 {
    let room = (i64::from(turn_model.context_window)
        - (request_chars as i64) / 3
        - i64::from(COMPACTION_SAFETY_BUFFER))
        .max(1) as u32;
    let mut tokens = room.min(SUMMARY_MAX_TOKENS_CEILING).max(SUMMARY_MAX_TOKENS);
    if turn_model.kind == crate::providers::ProviderKind::Anthropic
        && let Some(own) = turn_model.reported_max_output
    {
        tokens = tokens.min(own);
    }
    tokens
}

/// What the one continuation (SME-443) asks for: exactly what follows
/// the text above, never a repeat of it — the join is verbatim (Q3
/// decided). The request ends on `user`, so no provider needs prefill
/// support: a request *ending* on a partial assistant is the prefill
/// case; continuing on a trailing assistant message is the normal shape.
#[cfg(feature = "server")]
pub(super) const SUMMARY_CONTINUE_PROMPT: &str = "Continue the summary exactly where it stopped. \
    Write only what follows the text above, never repeat it; keep the same record and the \
    Done/Planned rule.";

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
/// oversized request compaction exists to prevent — see SME-18's
/// "Resolved" decision on this.
///
/// `refused`, when set, is why the turn needed this compaction rather
/// than waiting for the proactive check — an overflow refusal's reason
/// (from `overflow::refused_reason`), saved on the summary block so the
/// transcript divider reads it (SME-73). A compaction whose *own* summary
/// request was refused and recovered fills in its own reason when no one
/// else named one.
#[cfg(feature = "server")]
pub(super) async fn compact_conversation(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &mut crate::providers::TurnModel,
    slot: anthropic::stream::SlotPolicy<'_>,
    refused: Option<String>,
) -> ServerFnResult<()> {
    use tracing::field::Empty;
    // Its model call is its child (SME-137).
    let span = tracing::info_span!(
        "compaction",
        conversation_id,
        otel.status_code = Empty,
        otel.status_description = Empty,
    );
    crate::telemetry::in_span(span.clone(), async move {
        let result = compact(pool, conversation_id, turn_model, slot, refused).await;
        if let Err(e) = &result {
            crate::telemetry::mark_error(&span, &e.to_string());
        }
        result
    })
    .await
}

/// `compact_conversation`'s work, in its span.
async fn compact(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &mut crate::providers::TurnModel,
    slot: anthropic::stream::SlotPolicy<'_>,
    refused: Option<String>,
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

    let transcript = compaction_transcript(
        &messages,
        compaction_transcript_budget(turn_model.context_window, turn_model.kind),
    );

    // A summary request refused as an overflow (SME-73) is that same
    // refusal, and gets the same one answer here: learn the window it
    // names and ask once more with the transcript sized to it — half
    // the refused one when no window was named. A second refusal ends
    // the turn with the reason.
    let mut refused = refused;
    let summary = match summarize(pool, conversation_id, turn_model, slot, &transcript).await {
        Ok(summary) => summary,
        Err(anthropic::stream::ModelError::Overflow(overflow)) => {
            let named = super::overflow::learn(pool, conversation_id, turn_model, overflow.window).await;
            let budget = named.map_or(transcript.chars().count() / 2, |w| compaction_transcript_budget(w, turn_model.kind));
            let transcript = compaction_transcript(&messages, budget);
            match summarize(pool, conversation_id, turn_model, slot, &transcript).await {
                Ok(summary) => {
                    // The refusal this compaction answered, when its
                    // caller didn't name one: the divider reads the
                    // window this refusal learned (SME-73).
                    refused = refused.or_else(|| Some(super::overflow::refused_reason(named)));
                    summary
                }
                Err(anthropic::stream::ModelError::Overflow(again)) => {
                    super::overflow::learn(pool, conversation_id, turn_model, again.window).await;
                    let window = again
                        .window
                        .or(named)
                        .map(|w| format!(" ({} tokens)", super::overflow::grouped(u64::from(w))))
                        .unwrap_or_default();
                    return Err(ServerFnError::new(format!(
                        "Couldn't compact the conversation: even the summary request didn't fit \
                         `{model}`'s context window{window}. The server said: {said}",
                        model = turn_model.model,
                        said = again.message
                    )));
                }
                Err(e) => return Err(ServerFnError::new(e)),
            }
        }
        Err(e) => return Err(ServerFnError::new(e)),
    };

    let unanswered = cap_unanswered(unanswered_user_text(&messages), typed_budget(turn_model.context_window));
    let typed = typed_user_text(&messages, turn_model.context_window);
    for (role, content) in compaction_messages(summary, covers_through_message_id, &typed, &unanswered, refused) {
        let saved = db::create_message(pool, conversation_id, role, &content)
            .await
            .map_err(ServerFnError::new)?;
        // Watching tabs show the divider as it happens (SME-40 F5).
        record_saved(conversation_id, &mut Vec::new(), saved);
    }
    // The usage that triggered this described the old, long history. Kept,
    // a failed or stopped next call would have the following turn compact
    // again at once; the next real reply records the new size (SME-51 B10).
    db::clear_conversation_usage(pool, conversation_id)
        .await
        .map_err(ServerFnError::new)?;
    crate::claude_cli::session::reset(pool, conversation_id, turn_model.kind).await;
    Ok(())
}

/// The summarization call itself: a plain, tools-less request over the
/// transcript, in its own recorder (a refused attempt is recorded by
/// its drop, SME-178). Answers with the summary's text — for an HTTP
/// kind, after the one continuation its budget cut-off gets (SME-443),
/// marked where it honestly stopped — or with the typed error so the
/// caller can answer an overflow (SME-73).
async fn summarize(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &crate::providers::TurnModel,
    slot: anthropic::stream::SlotPolicy<'_>,
    transcript: &str,
) -> Result<String, anthropic::stream::ModelError> {
    let live_state = describe_live_state(pool, conversation_id).await;
    let prompt = format!(
        "Conversation so far:\n{transcript}\n\nCurrently live (preserve these \
         exact ids in your summary):\n{live_state}"
    );
    let request_chars = prompt.chars().count();

    let summarization_request = summary_request(
        turn_model,
        vec![anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text { text: prompt.clone() }],
        }],
        request_chars,
    );

    let mut discard_deltas = |_: &str| {};
    let recorder =
        CallRecorder::new(pool, conversation_id, turn_model, db::ModelCallKind::Compaction, &summarization_request);
    let turn = super::call::stream_for_kind(
        conversation_id, turn_model,
        &summarization_request,
        &mut discard_deltas,
        recorder.meter(),
        slot,
    )
    .await?;
    // Recorded as soon as it completes: the tokens are spent whether or
    // not the summary is saved below (SME-106). One stopped or failed
    // before then is recorded by the recorder's drop (SME-178).
    recorder.finish(&turn, None).await.map_err(|e| anthropic::stream::ModelError::Other(e.to_string()))?;
    let summary = summary_text(&turn);
    // On Claude Code, an empty or cut-off summary fails the compaction
    // rather than replace the history (PR #291 review 1 M1) — an HTTP
    // kind's decisions follow beside `checked` (SME-443).
    let summary = crate::claude_cli::summary::checked(turn_model.kind, summary, &turn.stop_reason)?;
    if turn_model.kind == crate::providers::ProviderKind::ClaudeCli {
        return Ok(summary);
    }
    match crate::claude_cli::summary::http_first(summary, &turn.stop_reason) {
        crate::claude_cli::summary::HttpStep::Save(text) | crate::claude_cli::summary::HttpStep::SaveMarked(text) => Ok(text),
        // The one continuation — an `if`, not a loop: no continuation ever
        // follows a continuation, so it can never spin (2c; `compact`'s
        // overflow retry is bounded too — see `continue_summary`).
        crate::claude_cli::summary::HttpStep::Continue(part1) => {
            continue_summary(pool, conversation_id, turn_model, slot, &prompt, &part1).await
        }
    }
}

/// The summarization request shape (SME-443): tools-less, thinking off,
/// and on HTTP kinds the room-derived budget. The first call and its one
/// continuation differ only in their messages — and in the request's
/// size, which the budget is derived from.
#[cfg(feature = "server")]
fn summary_request(
    turn_model: &crate::providers::TurnModel,
    messages: Vec<anthropic::AnthropicMessage>,
    request_chars: usize,
) -> anthropic::CreateMessageRequest {
    anthropic::CreateMessageRequest {
        model: turn_model.model.clone(),
        // HTTP kinds send the room-derived budget (SME-443); Claude Code
        // keeps the constant — its cap already carries the thinking that
        // can't be turned off (`once::THINKING_ROOM`), and SME-431 checks
        // its summary rather than growing it.
        max_tokens: if turn_model.kind == crate::providers::ProviderKind::ClaudeCli {
            SUMMARY_MAX_TOKENS
        } else {
            summary_max_tokens(turn_model, request_chars)
        },
        system: Some(COMPACTION_SYSTEM_PROMPT.to_string()),
        messages,
        stream: true,
        tools: vec![],
        thinking: None,
        prompt_caching: false,
        output_config: None,
        // Thinking off, said so a llama.cpp template doesn't reason
        // anyway and spend the summary's tokens on it (SME-111).
        chat_template_kwargs: turn_model.chat_template_kwargs(false),
    }
}

/// The reply's first text block — thinking is narration for the model
/// alone, never the saved record (SME-443: a strata-style body leads
/// with reasoning; only its Text is the summary).
#[cfg(feature = "server")]
fn summary_text(turn: &anthropic::stream::StreamedTurn) -> String {
    turn.content
        .iter()
        .find_map(|block| match block {
            anthropic::ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The one continuation an HTTP kind's budget-cut summary gets
/// (SME-443): same system prompt and thinking-off flags, messages
/// `[user(prompt), assistant(part1), user(SUMMARY_CONTINUE_PROMPT)]` —
/// ending on `user`, so no provider needs prefill support — and a
/// freshly derived budget for the larger request. Its own recorder, so
/// its cost is its own `compaction` usage row (SME-106). An overflow
/// from here propagates through `summarize` into `compact`'s overflow
/// arm like one from the first call (review 1's correction to 2c): the
/// window is learned and the half-sized transcript retried, and that
/// retry can itself continue — at most 4 `compaction` calls in one pass,
/// bounded, no loop. The 2-call bound describes the refusal path, whose
/// `Other` errors propagate and fail the turn with the history intact.
async fn continue_summary(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &crate::providers::TurnModel,
    slot: anthropic::stream::SlotPolicy<'_>,
    prompt: &str,
    part1: &str,
) -> Result<String, anthropic::stream::ModelError> {
    let message = |role: &str, text: &str| anthropic::AnthropicMessage {
        role: role.to_string(),
        content: vec![anthropic::ContentBlock::Text { text: text.to_string() }],
    };
    let request_chars = prompt.chars().count() + part1.chars().count() + SUMMARY_CONTINUE_PROMPT.chars().count();
    let continuation_request = summary_request(
        turn_model,
        vec![
            message("user", prompt),
            message("assistant", part1),
            message("user", SUMMARY_CONTINUE_PROMPT),
        ],
        request_chars,
    );
    let mut discard_deltas = |_: &str| {};
    let recorder =
        CallRecorder::new(pool, conversation_id, turn_model, db::ModelCallKind::Compaction, &continuation_request);
    let turn = super::call::stream_for_kind(
        conversation_id, turn_model,
        &continuation_request,
        &mut discard_deltas,
        recorder.meter(),
        slot,
    )
    .await?;
    // Recorded as soon as it completes: the tokens are spent whether or
    // not the join is saved (SME-106).
    recorder.finish(&turn, None).await.map_err(|e| anthropic::stream::ModelError::Other(e.to_string()))?;
    match crate::claude_cli::summary::http_continued(part1, summary_text(&turn), &turn.stop_reason) {
        crate::claude_cli::summary::ContinuedStep::Save(text) | crate::claude_cli::summary::ContinuedStep::SaveMarked(text) => Ok(text),
        // Cut off again: the compaction is refused, not saved — no
        // divider, no progress marked, the plan's at-most-2 bound
        // (SME-443 step 3; 2c).
        crate::claude_cli::summary::ContinuedStep::Fail(why) => Err(anthropic::stream::ModelError::Other(why)),
    }
}
