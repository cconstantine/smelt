//! A model call's request, saving its reply, and running the tools it
//! asks for: the steps of `run_turn_steps` (`run.rs`).

use super::*;

/// A model call's request: the history, the system prompt and the tools,
/// with a reply budget sized to the room the conversation has left.
#[cfg(feature = "server")]
pub(super) async fn build_request(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &crate::providers::TurnModel,
    last_known_usage: Option<&anthropic::TokenUsage>,
    pending_new_content: &[anthropic::ContentBlock],
) -> ServerFnResult<(anthropic::CreateMessageRequest, ReplyBudget)> {
    let history = history_for_request(
        db::list_messages(pool, conversation_id)
            .await
            .map_err(ServerFnError::new)?,
        turn_model.thinking_stripped_through,
    )
    .map_err(ServerFnError::new)?;

    let mut request = anthropic::CreateMessageRequest {
        model: turn_model.model.clone(),
        // Set below, once the request's size is known.
        max_tokens: MIN_REPLY_TOKENS,
        system: Some(system_prompt(
            &prompt_environment(pool, conversation_id, &turn_model.model).await,
        )),
        messages: history,
        stream: true,
        tools: anthropic::tools::tool_definitions(pool, conversation_id).await,
        // Set below, with the reply budget it's a share of.
        thinking: None,
        prompt_caching: turn_model.prompt_caching,
        output_config: turn_model.effort.map(|effort| anthropic::OutputConfig { effort }),
        chat_template_kwargs: turn_model.chat_template_kwargs(turn_model.thinking),
    };

    // As long as the conversation has room for, computed per
    // request: each tool round's from its own usage (SME-111).
    // Thinking shares it with the reply.
    let projected = projected_input(last_known_usage, estimate_tokens(pending_new_content))
        .unwrap_or_else(|| estimate_request_tokens(&request));
    let budget = reply_budget(turn_model.context_window, projected, turn_model.output_cap);
    request.max_tokens = budget.tokens;
    request.thinking = turn_model.thinking_config(request.max_tokens);
    // A budget with no room to think in turned thinking off: the
    // template is told too (SME-111 review 1).
    request.chat_template_kwargs = turn_model.chat_template_kwargs(request.thinking.is_some());
    Ok((request, budget))
}

/// Saves a model call's reply, and, for a reply cut off or refused, a
/// "not run" result for each whole call in it.
#[cfg(feature = "server")]
pub(super) async fn save_reply(
    pool: &PgPool,
    conversation_id: i64,
    turn: &anthropic::stream::StreamedTurn,
    persisted: &mut Vec<Message>,
) -> ServerFnResult<()> {
    let cut_off = anthropic::stream::is_cut_off(&turn.stop_reason);
    let refused = turn.stop_reason == "refusal";

    // An empty or blank reply has nothing to save: the API refuses
    // an empty assistant message once it isn't the last, so saving
    // one failed every later turn (B3, SME-122). A cut-off reply
    // that was nothing but the call it was cut in is one (SME-126).
    let reply = without_blank_text(turn.content.clone());
    if !reply.is_empty() {
        let saved = db::create_message(pool, conversation_id, "assistant", &reply)
            .await
            .map_err(ServerFnError::new)?;
        record_saved(conversation_id, persisted, saved);
    }
    // Saved (and about to be published as a message), so no longer
    // "in progress".
    clear_reply_in_progress(conversation_id);
    // A cut-off reply's whole calls don't run (its `stop_reason`
    // isn't `tool_use`), and each says so, rather than looking
    // finished with no result. Saved straight after the reply, so
    // a failure after it can't leave them to be answered as
    // stopped mid-run (SME-126 review 1).
    if cut_off || refused {
        let not_run: Vec<anthropic::ContentBlock> = turn
            .content
            .iter()
            .filter_map(|block| match block {
                anthropic::ContentBlock::ToolUse { id, .. } => Some(anthropic::ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: CUT_OFF_TOOL_CALL.to_string(),
                    is_error: Some(true),
                }),
                _ => None,
            })
            .collect();
        if !not_run.is_empty() {
            let saved = db::create_message(pool, conversation_id, "user", &not_run)
                .await
                .map_err(ServerFnError::new)?;
            record_saved(conversation_id, persisted, saved);
        }
    }
    Ok(())
}

/// The notice for a reply cut off at its budget, or refused, saved once,
/// after the reply, for the model and every tab.
///
/// Cut off at its budget: said once, after the reply, to the
/// model and to every tab (SME-111). Not continued: Claude
/// refuses a prefilled partial reply, and a local model would
/// start its reasoning over.
/// The context window stopping it (Claude 4.5+'s own stop
/// reason) is the room running out, whatever bound the budget
/// (SME-111 review 1).
/// The window stops a reply short of its budget: the notice
/// says what it wrote (SME-111 review 2).
/// The call a reply was cut in was dropped by the stream, and
/// the notice names it (SME-126).
#[cfg(feature = "server")]
pub(super) async fn save_stop_notice(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &crate::providers::TurnModel,
    turn: &anthropic::stream::StreamedTurn,
    budget: &ReplyBudget,
    persisted: &mut Vec<Message>,
) -> ServerFnResult<()> {
    let cut_off = anthropic::stream::is_cut_off(&turn.stop_reason);
    let refused = turn.stop_reason == "refusal";
    let cut_off_by = match turn.stop_reason.as_str() {
        _ if !cut_off => None,
        "model_context_window_exceeded" => Some((
            u32::try_from(turn.usage.output_tokens)
                .ok()
                .filter(|written| *written > 0)
                .unwrap_or(budget.tokens),
            crate::api::chat::ReplyLimit::RoomLeft,
        )),
        _ => Some((budget.tokens, match budget.limit {
            crate::api::chat::ReplyLimit::OutputCap if turn_model.output_cap_is_models_own => {
                crate::api::chat::ReplyLimit::ModelMaximum
            }
            limit => limit,
        })),
    };
    // A refused reply says so, the same way (B3, SME-122): it was
    // saved as if finished, and one refused in a call failed the
    // turn on the call's half-written input.
    let notice = match cut_off_by {
        Some((tokens, limit)) => Some(crate::api::chat::cut_off_notice(tokens, limit, turn.cut_off_call.as_deref())),
        None if refused => Some(crate::api::chat::refusal_notice(turn.cut_off_call.as_deref())),
        None => None,
    };
    if let Some(notice) = notice {
        let notice = [anthropic::ContentBlock::Text { text: notice }];
        let saved = db::create_message(pool, conversation_id, "user", &notice)
            .await
            .map_err(ServerFnError::new)?;
        record_saved(conversation_id, persisted, saved);
    }
    Ok(())
}

/// Runs a reply's tool calls and returns their results, and whether the
/// turn ends here: the reply asked the user a question, or an agent's
/// `finish` went through (SME-236), whose turn ends as `ask_user`'s does.
///
/// The model's `ask_user` (SME-34): one per reply; it gets no
/// result now, and the turn ends waiting on it. Recorded before
/// the reply's other tools run, so a Stop or restart while they
/// run leaves the question on its card (code review 2).
#[cfg(feature = "server")]
pub(super) async fn run_tools(
    pool: &PgPool,
    conversation_id: i64,
    turn: &anthropic::stream::StreamedTurn,
) -> ServerFnResult<(Vec<anthropic::ContentBlock>, bool)> {
    let mut result_blocks = Vec::new();
    // The question this reply asked, if any: `ask_user`'s or (SME-238)
    // `ask_parent`'s. One per reply, of either kind.
    let mut asked: Option<&str> = None;
    let mut finished = false;
    for block in &turn.content {
        if let anthropic::ContentBlock::ToolUse { id, name, input } = block
            && is_question(name)
        {
            let refused = match asked {
                Some(first) if first == name && name == crate::questions::ASK_USER => {
                    "ask one set of questions at a time: put them all in one ask_user call".to_string()
                }
                Some(_) => "ask one question at a time: ask_user and ask_parent each end your turn, so call \
                            only one of them, once"
                    .to_string(),
                None if name == crate::questions::ASK_USER => match crate::questions::parse_questions(input) {
                    Ok(questions) => {
                        ask(pool, conversation_id, id, questions).await?;
                        asked = Some(name);
                        continue;
                    }
                    Err(e) => e,
                },
                None => match crate::agents::ask_parent(pool, conversation_id, id, input).await {
                    Ok(()) => {
                        asked = Some(name);
                        continue;
                    }
                    Err(e) => e,
                },
            };
            result_blocks.push(anthropic::ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: refused,
                is_error: Some(true),
            });
        }
    }
    for block in &turn.content {
        if let anthropic::ContentBlock::ToolUse { id, name, input } = block
            && !is_question(name)
        {
            let result =
                anthropic::tools::execute(pool, conversation_id, id, name, input).await;
            if name == crate::agents::FINISH && result.is_ok() {
                finished = true;
            }
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
    Ok((result_blocks, asked.is_some() || finished))
}

/// Whether `name` is a question that ends the turn while it waits.
fn is_question(name: &str) -> bool {
    name == crate::questions::ASK_USER || name == crate::agents::ASK_PARENT
}

