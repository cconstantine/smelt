//! The turn's side of the model's `ask_user` (SME-34). The model's call
//! records a `pending_questions` row and ends the turn; while it waits,
//! turns save their notices and don't call the model; the card's answer is
//! written onto the row, and the next turn to hold the conversation's turn
//! lock takes it and sends it as the call's result, at the front of that
//! turn's message. A message the user writes instead takes the question
//! too, with `questions::NOT_ANSWERED` as the result.

use super::*;
use crate::questions::{PendingQuestion, Question, QuestionAnswer};

/// What `answer_question` says when the question isn't waiting any more:
/// another tab answered it, or the user wrote a message instead.
pub(crate) const ALREADY_ANSWERED: &str = "This question was already answered.";

/// Saves a turn's message (none for a wake), taking the conversation's
/// question first: an answered one always, and a waiting one when this is
/// the user's own message (`from_user`). The question's result goes at the
/// front. Holds the turn lock (the caller's). Returns what was saved and
/// its content, or `None` when there was nothing to save.
pub(super) async fn save_turn_message(
    pool: &PgPool,
    conversation_id: i64,
    new_message: Option<anthropic::AnthropicMessage>,
    from_user: bool,
) -> ServerFnResult<Option<(Message, Vec<anthropic::ContentBlock>)>> {
    let include_waiting = from_user && new_message.is_some();
    let (role, content) = match new_message {
        Some(message) => (message.role, message.content),
        None => ("user".to_string(), Vec::new()),
    };
    let saved = db::create_message_taking_question(pool, conversation_id, &role, content, include_waiting, question_result)
        .await
        .map_err(ServerFnError::new)?;
    let Some((message, question)) = saved else {
        return Ok(None);
    };
    if question.is_some() {
        publish_question(conversation_id, None);
    }
    let blocks = message.blocks().map_err(ServerFnError::new)?;
    Ok(Some((message, blocks)))
}

/// The `ask_user` call's result for `question`: the user's answer, or that
/// they wrote a message instead.
fn question_result(question: &db::StoredQuestion) -> anthropic::ContentBlock {
    let content = match &question.answer {
        Some(answers) => crate::questions::answered_result(&question.questions, answers),
        None => crate::questions::NOT_ANSWERED.to_string(),
    };
    anthropic::ContentBlock::ToolResult {
        tool_use_id: question.tool_use_id.clone(),
        content,
        is_error: None,
    }
}

/// Run by a turn right before it calls the model, holding the turn lock:
/// takes an answered question (saving the call's result as a message of
/// its own, which `move_late_results` puts after the call), and says
/// whether a question still waits, in which case the turn stops there.
/// Repeats until no row is left or one is waiting, since an answer can be
/// recorded between the take and the check.
pub(super) async fn take_answer_or_wait(
    pool: &PgPool,
    conversation_id: i64,
    pending_new_content: &mut Vec<anthropic::ContentBlock>,
    persisted: &mut Vec<Message>,
) -> ServerFnResult<bool> {
    loop {
        if let Some((saved, content)) = save_turn_message(pool, conversation_id, None, false).await? {
            pending_new_content.extend(content);
            record_saved(conversation_id, persisted, saved);
        }
        match db::get_pending_question(pool, conversation_id)
            .await
            .map_err(ServerFnError::new)?
        {
            None => return Ok(false),
            Some(question) if question.answer.is_none() => return Ok(true),
            Some(_) => continue,
        }
    }
}

/// Records the model's `ask_user` call `tool_use_id`: the conversation now
/// waits on `questions`.
pub(super) async fn ask(
    pool: &PgPool,
    conversation_id: i64,
    tool_use_id: &str,
    questions: Vec<Question>,
) -> ServerFnResult<()> {
    db::create_pending_question(pool, conversation_id, tool_use_id, &questions)
        .await
        .map_err(ServerFnError::new)?;
    publish_question(
        conversation_id,
        Some(PendingQuestion {
            tool_use_id: tool_use_id.to_string(),
            questions,
        }),
    );
    Ok(())
}

/// Tells every tab on the conversation which question it waits on, and
/// every tab's sidebar that the set of waiting conversations changed.
fn publish_question(conversation_id: i64, question: Option<PendingQuestion>) {
    crate::events::publish(conversation_id, crate::events::ConversationEvent::QuestionUpdate { question });
    crate::events::publish_app(crate::events::AppEvent::QuestionsChanged);
}

/// The question `conversation_id` waits on, for the card; `None` once
/// answered, even before a turn has taken the answer.
pub(crate) async fn pending_question(pool: &PgPool, conversation_id: i64) -> ServerFnResult<Option<PendingQuestion>> {
    let question = db::get_pending_question(pool, conversation_id)
        .await
        .map_err(ServerFnError::new)?;
    Ok(question
        .filter(|q| q.answer.is_none())
        .map(|q| PendingQuestion { tool_use_id: q.tool_use_id, questions: q.questions }))
}

/// The card's answer to question `tool_use_id`: checked, recorded once
/// (another tab's later answer gets `ALREADY_ANSWERED`), then a turn takes
/// it. Ends a Stop's pause, like the user writing does.
pub(crate) async fn answer_question(
    pool: PgPool,
    conversation_id: i64,
    tool_use_id: String,
    answers: Vec<QuestionAnswer>,
) -> ServerFnResult<()> {
    let Some(question) = db::get_pending_question(&pool, conversation_id)
        .await
        .map_err(ServerFnError::new)?
        .filter(|q| q.answer.is_none() && q.tool_use_id == tool_use_id)
    else {
        return Err(ServerFnError::new(ALREADY_ANSWERED));
    };
    crate::questions::validate_answers(&question.questions, &answers).map_err(ServerFnError::new)?;
    if !db::answer_pending_question(&pool, conversation_id, &tool_use_id, &answers)
        .await
        .map_err(ServerFnError::new)?
    {
        return Err(ServerFnError::new(ALREADY_ANSWERED));
    }
    publish_question(conversation_id, None);
    resume_turns(conversation_id);
    new_turn_generation(conversation_id);
    tokio::spawn(async move {
        if let Err(e) = run_turn_bounded(&pool, conversation_id, None, MAX_TURNS, true, false).await {
            let message = e.message();
            crate::events::publish(conversation_id, crate::events::ConversationEvent::TurnError { message });
        }
    });
    Ok(())
}
