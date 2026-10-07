use dioxus::fullstack::ServerEvents;
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

use crate::models::{Conversation, Message};
use crate::{anthropic, events};

#[cfg(feature = "server")]
use crate::db;
#[cfg(feature = "server")]
use crate::turn::*;
#[cfg(feature = "server")]
use sqlx::PgPool;

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
    // Its turns first, so nothing is still making a pod (SME-51 B5).
    stop_turn_now(id);
    let _ = crate::browsing::close_session(id).await;
    // Read before the delete cascades them away: teardown deletes the pods
    // they name too, labelled or not (SME-88). Best-effort, like teardown.
    let pod_ids: Vec<i64> = match db::list_sandbox_pods(db::get(), id).await {
        Ok(rows) => rows.into_iter().map(|row| row.id).collect(),
        Err(e) => {
            tracing::warn!(conversation_id = id, error = %e, "couldn't list a conversation's pods before deleting it");
            Vec::new()
        }
    };
    db::delete_conversation(db::get(), id)
        .await
        .map_err(ServerFnError::new)?;
    // After the delete: a pod started meanwhile is found by its label, and
    // one still starting sees the conversation gone (`create_pod`).
    // Best-effort, unconditional (unlike terminate_pod, which the model
    // calls and which is guarded).
    crate::sandbox::teardown_conversation(id, &pod_ids).await;
    crate::events::forget(id);
    forget_conversation_lock(id);
    // After the delete, so a listener refetching sees the pod rows gone.
    crate::events::publish_app(crate::events::AppEvent::PodsChanged);
    Ok(())
}

/// Which conversations have a turn running, for the sidebar. Kept current
/// by `AppEvent::TurnsChanged`, relayed as `ConversationEvent::TurnsChanged`.
#[get("/api/conversations/busy")]
pub async fn get_busy_conversations() -> ServerFnResult<Vec<i64>> {
    Ok(busy_conversations())
}

/// The error a stopped turn ends with. Not server-only: the chat page
/// recognizes it to show "Stopped." instead of an error.
pub const TURN_STOPPED: &str = "stopped by the user";

/// Saved in the conversation when the user stops a turn, so the model (and
/// a compaction's continuation) knows the request before it was called
/// off, rather than taking it as still waiting for an answer (SME-51 B10).
/// Not server-only: the chat page shows it as "Stopped.".
pub const STOP_NOTICE: &str = "The user stopped this turn before it finished. Don't carry on with what it asked unless they ask again.";

/// The conversation's last failed turn's error, if the user hasn't written
/// since: the reconnect pull's copy of `ConversationEvent::TurnError`.
#[get("/api/conversations/{id}/turn-error")]
pub async fn get_turn_error(id: i64) -> ServerFnResult<Option<String>> {
    Ok(last_turn_error(id))
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

/// One-shot pull of the current todo list for the browser, used for the
/// todo panel's initial load and the reconciliation pull on
/// connect/reconnect (a `broadcast` channel has no replay).
#[get("/api/conversations/{id}/todos")]
pub async fn get_todos(id: i64) -> ServerFnResult<Vec<anthropic::tools::TodoItem>> {
    db::get_conversation_todos(db::get(), id)
        .await
        .map_err(ServerFnError::new)
}

/// How full the model's context window is right now — the always-visible
/// indicator's data. `usage` is `None` for a conversation with no
/// completed turn yet (nothing to report). See
/// SME-18.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ContextUsageSnapshot {
    pub usage: Option<anthropic::TokenUsage>,
    pub context_window: u32,
}

/// Which model the conversation's next turn uses, for the picker above the
/// message box (SME-72).
#[get("/api/conversations/{id}/model")]
pub async fn get_conversation_model(id: i64) -> ServerFnResult<crate::providers::ConversationModel> {
    crate::providers::conversation_model(db::get(), id)
        .await
        .map_err(ServerFnError::new)?
        .ok_or_else(|| ServerFnError::new("conversation not found"))
}

/// The picker's choice: the conversation's turns run on this provider and
/// model from the next one on.
#[post("/api/conversations/{id}/model")]
pub async fn set_conversation_model(id: i64, provider_id: i64, model: String) -> ServerFnResult<()> {
    crate::providers::set_conversation_model(db::get(), id, provider_id, &model)
        .await
        .map_err(ServerFnError::new)
}

/// One-shot pull for the always-visible context-usage indicator — same
/// shape as `get_todos`/`get_sandbox_state`.
#[get("/api/conversations/{id}/context-usage")]
pub async fn get_context_usage(id: i64) -> ServerFnResult<ContextUsageSnapshot> {
    let pool = db::get();
    let usage = db::get_conversation_usage(pool, id)
        .await
        .map_err(ServerFnError::new)?;
    Ok(ContextUsageSnapshot {
        usage,
        context_window: crate::providers::conversation_context_window(pool, id).await,
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
    /// The `AGENTS.md` files in `system`, with where each came from
    /// (SME-32).
    pub instructions: Vec<crate::git::ProjectInstructions>,
    pub tools: Vec<anthropic::ToolDefinition>,
    pub message_count: usize,
    pub usage: Option<anthropic::TokenUsage>,
    pub context_window: u32,
    /// Every completed model call so far, with its cost (SME-106).
    pub spend: crate::models::ConversationSpend,
    /// When the price catalog was last fetched; `None` before the first.
    pub prices_as_of: Option<chrono::NaiveDateTime>,
}

#[get("/api/conversations/{id}/context-detail")]
pub async fn get_context_detail(id: i64) -> ServerFnResult<ContextDetailSnapshot> {
    context_detail(db::get(), id).await
}

/// `get_context_detail`'s body, taking the pool so tests can call it.
#[cfg(feature = "server")]
pub(crate) async fn context_detail(pool: &PgPool, id: i64) -> ServerFnResult<ContextDetailSnapshot> {
    let tools = anthropic::tools::tool_definitions(pool).await;
    let message_count = db::list_messages(pool, id)
        .await
        .map_err(ServerFnError::new)?
        .len();
    let usage = db::get_conversation_usage(pool, id)
        .await
        .map_err(ServerFnError::new)?;
    let (model, context_window) = crate::providers::conversation_model(pool, id)
        .await
        .map_err(ServerFnError::new)?
        .as_ref()
        .and_then(crate::providers::ConversationModel::choice)
        .map_or((String::new(), crate::providers::ASSUMED_CONTEXT_WINDOW), |choice| {
            (choice.model.clone(), choice.context_window)
        });
    Ok(ContextDetailSnapshot {
        system: Some(system_prompt(&prompt_environment(pool, id, &model).await)),
        instructions: crate::git::project_instructions(pool, id)
            .await
            .map_err(ServerFnError::new)?,
        tools,
        message_count,
        usage,
        context_window,
        spend: db::get_conversation_spend(pool, id)
            .await
            .map_err(ServerFnError::new)?,
        prices_as_of: crate::pricing::CATALOG.current().map(|c| c.fetched_at),
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
    Ok(ServerEvents::from_stream(open_conversation_events(db::get(), id).await?))
}

/// `id`'s event stream (`conversation_event_stream`), refused for a
/// conversation that doesn't exist: a tab still reconnecting to a deleted
/// one would otherwise make it a channel again, for good (SME-91).
/// Checked again once subscribed, for a delete landing in between; the
/// refused stream's subscription frees the channel it made.
#[cfg(feature = "server")]
pub(crate) async fn open_conversation_events(
    pool: &PgPool,
    id: i64,
) -> ServerFnResult<impl futures_util::Stream<Item = Result<events::ConversationEvent, axum::BoxError>> + use<>> {
    let exists = || async { db::conversation_exists(pool, id).await.map_err(ServerFnError::new) };
    if !exists().await? {
        return Err(ServerFnError::new("conversation not found"));
    }
    let stream = conversation_event_stream(id);
    if !exists().await? {
        return Err(ServerFnError::new("conversation not found"));
    }
    Ok(stream)
}

/// Everything a tab watching `id` hears: that conversation's events, plus
/// the app-wide `PodsChanged`, `TurnsChanged`, `QuestionsChanged` and `ProvidersChanged`, relayed as their
/// `ConversationEvent` namesakes for the sidebar.
/// Ends when the conversation's channel closes (it was deleted).
#[cfg(feature = "server")]
pub(crate) fn conversation_event_stream(
    id: i64,
) -> impl futures_util::Stream<Item = Result<events::ConversationEvent, axum::BoxError>> {
    use tokio::sync::broadcast::error::RecvError;
    // Both subscribed now, not on first poll, so nothing published in
    // between is missed.
    let receivers = (events::subscribe(id), events::subscribe_app());
    futures_util::stream::unfold(receivers, move |(mut conversation, mut app)| async move {
        loop {
            tokio::select! {
                received = conversation.recv() => match received {
                    Ok(event) => return Some((Ok::<_, axum::BoxError>(event), (conversation, app))),
                    // A subscriber that fell behind has missed events it
                    // can't get back, a saved message or the turn ending
                    // among them. Ending the stream makes the tab reconnect
                    // and pull the current state (SME-51 B3).
                    Err(RecvError::Lagged(skipped)) => {
                        tracing::info!(conversation_id = id, skipped, "a tab fell behind; ending its stream so it resyncs");
                        return None;
                    }
                    Err(RecvError::Closed) => return None,
                },
                received = app.recv() => match received {
                    Ok(events::AppEvent::PodsChanged) => {
                        let event = events::ConversationEvent::PodsChanged {};
                        return Some((Ok(event), (conversation, app)));
                    }
                    Ok(events::AppEvent::TurnsChanged) => {
                        let event = events::ConversationEvent::TurnsChanged {};
                        return Some((Ok(event), (conversation, app)));
                    }
                    Ok(events::AppEvent::ProvidersChanged) => {
                        let event = events::ConversationEvent::ProvidersChanged {};
                        return Some((Ok(event), (conversation, app)));
                    }
                    Ok(events::AppEvent::QuestionsChanged) => {
                        let event = events::ConversationEvent::QuestionsChanged {};
                        return Some((Ok(event), (conversation, app)));
                    }
                    // Only ever decoded, never published.
                    Ok(events::AppEvent::Unknown) => continue,
                    // Missing some just means one refetch covers several.
                    Err(RecvError::Lagged(_)) => continue,
                    // The app-wide channel lives as long as the process.
                    Err(RecvError::Closed) => return None,
                },
            }
        }
    })
}
