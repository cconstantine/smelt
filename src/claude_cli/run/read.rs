//! Reading a run's reply (SME-430): the CLI's stdout lines, mapped
//! through `anthropic::stream`'s block assembler, its other events noted,
//! and the state the reply leaves the run in.

use serde_json::Value;

use super::*;

impl Run {
    /// The next line from the CLI, the bridge's calls taken as they come.
    pub(super) async fn next_event(&mut self) -> Result<Value, String> {
        loop {
            // Lines before calls, once both wait (SME-447): a line may be
            // the CLI's local refusal of a name it was never given, and
            // reading it is what lets the *next* reply's bridge call be
            // held for its result rather than refused as a stray (review 1
            // L1) — taking that call first would judge it while the slip's
            // un-taken result still fills `answers`, which is the very test
            // L1 reads as "the CLI is still making that reply's calls". The
            // order is the CLI's own: its refusal echo goes out *before* it
            // issues its own next API call, so a call racing an echo's line
            // was issued after that line went out, and reading the line
            // first is the order reality gives. A call then waits only
            // until the next poll with nothing left to read; the
            // quiet-stdout case is unaffected.
            tokio::select! { biased;
                line = tokio::time::timeout(silence(), self.lines.next_line()) => {
                    let line = match line {
                        Err(_) => {
                            self.healthy = false;
                            if !self.reply_started {
                                self.failure = Failure::Unchanged;
                            }
                            return Err(format!("Claude Code said nothing for {} seconds, so smelt stopped it.", silence().as_secs()));
                        }
                        Ok(Ok(Some(line))) => line,
                        Ok(Ok(None)) | Ok(Err(_)) => {
                            self.healthy = false;
                            self.gone = true;
                            let status = tokio::time::timeout(WIND_DOWN, self.child.wait()).await;
                            let code = match status {
                                Ok(Ok(status)) => status.to_string(),
                                _ => "an unknown status".to_string(),
                            };
                            // The whole tail goes to the server's log only; the
                            // turn's error, which the conversation keeps, gets
                            // one short scrubbed line of it (review 1 L4).
                            let tail = self.stderr_tail();
                            if !tail.is_empty() {
                                tracing::warn!(stderr = %tail, "Claude Code exited before it finished");
                            }
                            let line = crate::claude_cli::errors::scrubbed_line(&tail);
                            let said = if line.is_empty() { String::new() } else { format!(" It said: {line}") };
                            return Err(format!("Claude Code exited ({code}) before it finished.{said}"));
                        }
                    };
                    // A line that isn't a JSON object isn't the CLI's protocol.
                    if let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(&line) {
                        self.heard = true;
                        // Started: done with the start lock (L11).
                        self.launching.release();
                        return Ok(value);
                    }
                }
                Some(parked) = self.bridge.calls.recv() => self.take_call(parked),
            }
        }
    }

    /// Reads one API call's reply.
    pub(super) async fn read_reply(&mut self, on_delta: &mut (dyn FnMut(&str) + Send), meter: &UsageMeter, extra: &UsageMeter) -> Result<StreamedTurn, String> {
        let mut blocks = Blocks::default();
        let mut stop_reason = String::new();
        self.streaming_ids.clear();
        loop {
            let event = self.next_event().await?;
            match event["type"].as_str() {
                Some("stream_event") => {
                    let inner = &event["event"];
                    match inner["type"].as_str() {
                        // A new API call: the last reply's results are all in.
                        Some("message_start") => {
                            self.delivered = false;
                            self.reply_started = true;
                        }
                        Some("content_block_start") if inner["content_block"]["type"] == "tool_use" => {
                            if let Some(id) = inner["content_block"]["id"].as_str() {
                                self.streaming_ids.insert(id.to_string());
                            }
                        }
                        _ => {}
                    }
                    if inner["type"] == "message_stop" {
                        return self.finish(blocks, stop_reason, meter, extra).await;
                    }
                    if let Err(e) = apply(&mut blocks, &mut stop_reason, inner, on_delta, meter) {
                        self.healthy = false;
                        return Err(e);
                    }
                }
                Some("system") if event["subtype"] == "init" && self.has_tools && !bridge_connected(&event) => {
                    self.healthy = false;
                    return Err("Claude Code couldn't connect to smelt's tools, so smelt stopped the call rather than let it run without them. Send another message to try again; if this keeps happening, the server's log says why.".to_string());
                }
                Some("user") => {
                    let answers = self.answered_by_the_cli(&event);
                    // An answer to a call of a name the CLI was given, or
                    // an echo with no `init` list to check: its context and
                    // smelt's no longer agree (review 1 L2).
                    if let Some(id) = answers.iter().find_map(|a| match a {
                        CliAnswered::Diverged { id } => Some(id),
                        CliAnswered::Slip { .. } => None,
                    }) {
                        self.healthy = false;
                        return Err(format!("Claude Code answered tool call {id} itself, so its context and smelt's no longer agree; smelt stopped the call. Send another message to carry on from smelt's record."));
                    }
                    // A name the CLI was never given is its own local
                    // unknown-name refusal (SME-447): the slip an HTTP kind
                    // gets smelt's correction for. Warn — never quoting the
                    // CLI's wording, which changes between versions — and
                    // carry on reading: the reply's call goes through
                    // `run_tools` and counts toward the shared unknown-name
                    // bound, as on HTTP.
                    for answer in answers {
                        if let CliAnswered::Slip { id, name } = answer {
                            // The answer `send_tail` queued for it waits on
                            // a bridge call the CLI will never make: drop it,
                            // so `answers` keeps meaning "results the CLI may
                            // still take" — a stale entry there makes a real
                            // call arriving mid-`stream` read as a stray
                            // (review 1 L1). The record carries the correction.
                            self.answers.remove(&id);
                            tracing::warn!(conversation_id = self.conversation_id, tool_use_id = %id, tool = %name, "Claude Code refused a call to a name it was never given; smelt carries on and answers it from its own record");
                        }
                    }
                }
                Some("result") => {
                    self.learn_window(&event);
                    self.state = State::Idle;
                    self.healthy = false;
                    if !self.reply_started {
                        self.failure = if session_missing(&event) { Failure::SessionMissing } else { Failure::Unchanged };
                    }
                    // "Prompt is too long": the turn's recovery compacts (SME-431).
                    if let Some(crate::anthropic::stream::ModelError::Overflow(overflow)) = crate::claude_cli::once::result_failure(&event, self.rate.as_ref()) {
                        self.overflow = Some(overflow);
                    }
                    return Err(result_error(&event, self.rate.as_ref())
                        .unwrap_or_else(|| "Claude Code ended the reply before it finished.".to_string()));
                }
                _ => self.note(&event),
            }
        }
    }

    /// The CLI's echoed `tool_result`s for calls of the parked reply that
    /// smelt never answered, one per block — the CLI can echo several in
    /// one event, and now that a slip carries on, a second block must not
    /// hide behind the first (review 1 L2, SME-447).
    pub(super) fn answered_by_the_cli(&self, event: &Value) -> Vec<CliAnswered> {
        let Some(blocks) = event["message"]["content"].as_array() else { return Vec::new() };
        blocks
            .iter()
            .filter_map(|block| {
                if block["type"] != "tool_result" {
                    return None;
                }
                let id = block["tool_use_id"].as_str()?;
                let ours = self.expected.iter().find(|e| e.id == id)?;
                if self.answered.contains(id) {
                    return None;
                }
                // The name decides, never the wording. A call of a tool
                // this run handed the CLI — membership in `register`'s own
                // map, the record of what the CLI was given (review 1 L1:
                // never a re-derivation, which parts company with
                // `register`'s repeated strip for a name starting with the
                // CLI's own prefix) — can only be an answer the bridge
                // never saw: divergence. Of a name it was never given, the
                // echo can only be its local unknown-name refusal — with
                // no `init` list to prove the run checkable, keep today's
                // loud failure.
                Some(if self.cli_tools.is_some() && !self.bridge.was_handed(&ours.name) {
                    CliAnswered::Slip { id: id.to_string(), name: ours.name.clone() }
                } else {
                    CliAnswered::Diverged { id: id.to_string() }
                })
            })
            .collect()
    }

    /// The window and reply cap a `result` reports for the run's model
    /// (`modelUsage[model].contextWindow`, `maxOutputTokens`), kept for
    /// `call` to save (SME-431). A result with an unknown or implausible
    /// window keeps what was there.
    pub(super) fn learn_window(&mut self, result: &Value) {
        let Some(models) = result["modelUsage"].as_object() else { return };
        let usage = models.get(&self.model).or_else(|| models.values().find(|u| u["canonicalModel"] == self.model.as_str()));
        let Some(usage) = usage else { return };
        let read = |key: &str| usage[key].as_i64().and_then(|n| i32::try_from(n).ok()).filter(|n| *n > 0);
        if let Some(window) = read("contextWindow").filter(|w| *w >= 1_024) {
            self.reported = Some((window, read("maxOutputTokens")));
        }
    }

    /// What smelt notes of a line that isn't the reply's.
    pub(super) fn note(&mut self, event: &Value) {
        match (event["type"].as_str(), event["subtype"].as_str()) {
            (Some("rate_limit_event"), _) => self.rate = Some(RateLimit::from_event(event)),
            (Some("system"), Some("init")) => {
                // What this process's CLI says it can call (SME-447), the
                // witness that echoes are checkable at all. A non-empty
                // array of strings only: an empty one from a `connected`
                // init contradicts the arm that admitted it (a turn always
                // has tools) and is no list to check against — uncheckable
                // stays today's loud divergence (review 1 L2).
                if let Some(tools) = event["tools"].as_array().filter(|tools| !tools.is_empty())
                    && let Some(names) = tools.iter().map(|t| t.as_str()).collect::<Option<Vec<&str>>>()
                {
                    self.cli_tools = Some(names.into_iter().map(str::to_string).collect());
                }
            }
            (Some("system"), Some("api_retry")) => {
                tracing::info!(attempt = event["attempt"].as_i64(), error = event["error"].as_str(), "Claude Code is retrying an API call");
            }
            (Some("system"), Some("compact_boundary")) => {
                // DISABLE_COMPACT is set, so this shouldn't come; smelt's
                // history stays the record, and the next call starts over.
                tracing::warn!("Claude Code compacted its context though smelt turned that off: the next call starts a new run");
                self.healthy = false;
            }
            _ => {}
        }
    }

    /// The reply at its `message_stop`.
    pub(super) async fn finish(&mut self, blocks: Blocks, stop_reason: String, meter: &UsageMeter, extra: &UsageMeter) -> Result<StreamedTurn, String> {
        let (mut content, cut_off_call) = blocks.finish(&stop_reason)?;
        for block in &mut content {
            if let ContentBlock::ToolUse { name, .. } = block
                && let Some(smelt) = self.bridge.smelt_name(name)
            {
                *name = smelt.to_string();
            }
        }
        let turn = StreamedTurn { content, stop_reason, usage: meter.usage(), cut_off_call };
        if turn.stop_reason == "tool_use" {
            self.expected = turn
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, input } => Some(Expected { id: id.clone(), name: name.clone(), input: input.clone() }),
                    _ => None,
                })
                .collect();
            self.state = State::Parked;
            self.settle_held();
            return Ok(turn);
        }
        if crate::anthropic::stream::is_cut_off(&turn.stop_reason) {
            // The CLI would call again on its own (S6): stop it now.
            if self.interrupt().await.is_err() {
                self.healthy = false;
            }
        }
        self.settle_held();
        self.await_result(extra).await;
        Ok(turn)
    }

    /// Reads to the run's `result`, within `WIND_DOWN`: an API call the
    /// CLI made on its own meanwhile is metered on `extra` and otherwise
    /// dropped. A run that doesn't get there isn't used again.
    pub(super) async fn await_result(&mut self, extra: &UsageMeter) {
        let waited = tokio::time::timeout(WIND_DOWN, async {
            loop {
                let event = self.next_event().await?;
                match event["type"].as_str() {
                    Some("result") => {
                        self.learn_window(&event);
                        return Ok::<_, String>(());
                    }
                    Some("stream_event") => {
                        let inner = &event["event"];
                        match interpret_stream_event(inner) {
                            StreamOutcome::InitialUsage(usage) => extra.start(&usage),
                            StreamOutcome::Delta { usage, .. } => extra.delta(&usage),
                            StreamOutcome::TextDelta(text) | StreamOutcome::ThinkingDelta(text) => extra.streamed(text.len()),
                            _ => {}
                        }
                    }
                    _ => self.note(&event),
                }
            }
        })
        .await;
        if !matches!(waited, Ok(Ok(()))) {
            self.healthy = false;
        }
        self.state = State::Idle;
        self.expected.clear();
    }
}

/// One `stream_event`'s API event, applied to the reply as
/// `anthropic::stream` applies an SSE event.
pub(in crate::claude_cli) fn apply(blocks: &mut Blocks, stop_reason: &mut String, event: &Value, on_delta: &mut (dyn FnMut(&str) + Send), meter: &UsageMeter) -> Result<(), String> {
    let index = block_index(event);
    match interpret_stream_event(event) {
        StreamOutcome::TextDelta(text) => {
            meter.streamed(text.len());
            on_delta(&text);
            blocks.text(index, text);
        }
        StreamOutcome::ThinkingDelta(thinking) => {
            meter.streamed(thinking.len());
            blocks.thinking(index, thinking);
        }
        StreamOutcome::ThinkingSignatureDelta(signature) => blocks.signature(index, &signature),
        StreamOutcome::ToolUseStart { id, name } => blocks.start(index, PartialBlock::ToolUse { id, name, partial_json: String::new() }),
        StreamOutcome::ToolUseInputDelta(partial_json) => {
            meter.streamed(partial_json.len());
            blocks.tool_input(index, &partial_json);
        }
        StreamOutcome::BlockStop => blocks.stop(index),
        StreamOutcome::Delta { stop_reason: reason, usage } => {
            if let Some(reason) = reason {
                *stop_reason = reason;
            }
            meter.delta(&usage);
        }
        StreamOutcome::InitialUsage(initial) => meter.start(&initial),
        StreamOutcome::Error(message) => return Err(format!("Claude Code's API call failed: {message}")),
        StreamOutcome::Ignored => {
            // A thinking block opens with an empty start event, which the
            // HTTP stream also ignores: its first delta opens it.
        }
    }
    Ok(())
}

/// What an echoed `tool_result` for an expected call smelt never answered
/// is (SME-447): `Slip`, the CLI's own local refusal of a name it was never
/// given — the same mistake an HTTP kind gets smelt's unknown-call
/// correction for, so the turn carries on and the shared bound counts it;
/// or `Diverged`, an answer to a call the CLI was given, or an echo with no
/// `init` tool list to check against, where the two contexts no longer
/// agree (review 1 L2, unchanged).
pub(super) enum CliAnswered {
    Slip { id: String, name: String },
    Diverged { id: String },
}

/// Whether a `result` says the session to resume doesn't exist, as the
/// real CLI says it (the capture `missing`).
fn session_missing(result: &Value) -> bool {
    let said = |text: &str| text.contains("No conversation found with session ID");
    result["errors"].as_array().is_some_and(|errors| errors.iter().filter_map(Value::as_str).any(said)) || result["result"].as_str().is_some_and(said)
}

/// Whether an `init` event says the CLI has smelt's tools: the bridge
/// `connected`, or any of its tools listed. The CLI waits about 30 s for
/// a server before it starts, then starts with it `pending` and none of its
/// tools (the capture `pending`, review 2 L15), so `pending` alone isn't
/// enough, and its tools are (review 1 L7).
fn bridge_connected(init: &Value) -> bool {
    let name = crate::claude_cli::bridge::SERVER_NAME;
    let connected = init["mcp_servers"].as_array().is_some_and(|servers| servers.iter().any(|s| s["name"] == name && s["status"] == "connected"));
    let prefix = format!("mcp__{name}__");
    connected || init["tools"].as_array().is_some_and(|tools| tools.iter().any(|t| t.as_str().is_some_and(|t| t.starts_with(&prefix))))
}
