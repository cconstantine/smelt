//! One turn's CLI run (SME-430): a `claude -p` process held for the whole
//! turn, each of the turn's model calls one of its API calls.
//!
//! States (SME-399's state model): **None** (no run), **Streaming** (an
//! API call in flight), **Parked** (the reply ended in `tool_use`; its
//! calls wait in the bridge for smelt's results), **Idle** (`result`
//! seen, stdin open). A call that fails ends the run, and the next call
//! starts a new one, given smelt's history as text.
//!
//! - call, None: spawn, send the history (`render::rehydrate`) → Streaming.
//! - call, Parked: answer each of the reply's calls with its result, as
//!   the call arrives (the CLI makes them one at a time, S2), the tail's
//!   other blocks as text on the last → Streaming.
//! - call, Idle: send the tail as a user message → Streaming.
//! - call, with another reply cap than the run's launch (the CLI reads it
//!   once): end the run, then as None (review 1 L3).
//! - call, the history went another way (`render::tail_after`): end the
//!   run, then as None.
//! - `message_stop` `tool_use` → Parked, return the reply.
//! - `message_stop` cut off (`max_tokens`): interrupt at once, since the
//!   CLI would carry on by itself (S6); its continuation's usage goes to
//!   the extra meter; await `result` → Idle, return the reply.
//! - `message_stop` otherwise: await `result` → Idle, return the reply.
//! - call, the process gone (it exited while smelt ran a tool, or before
//!   it said anything to this call): end the run, then as None, the
//!   reply's results going to the new run as text (review 1 M1).
//! - `result` with `is_error` before `message_stop`, the process ending,
//!   or a silence over `SILENCE`: the call fails, and the run ends.
//! - `TurnCli` dropped (the turn finished, failed, stopped or asked the
//!   user): Streaming or Parked → interrupt (the CLI then records each
//!   parked call as stopped, the planner's P3), then close stdin, wait,
//!   kill; Idle → close stdin, wait, kill. In a spawned task, so a Stop
//!   never waits for it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

use super::bridge::{ParkedCall, RunBridge, ToolReply, bridge};
use super::errors::{RateLimit, result_error};
use super::process::{Launch, SessionArg, cli_args, turn_env};
use super::render::{Synced, blocks_text, call_names, rehydrate, synced, tail_after, tool_result_text};
use crate::anthropic::stream::interpret::{StreamOutcome, block_index, interpret_stream_event};
use crate::anthropic::stream::{Blocks, PartialBlock, StreamedTurn, UsageMeter};
use crate::anthropic::{AnthropicMessage, ContentBlock, CreateMessageRequest};

/// The longest the CLI may say nothing while smelt waits on it, before the
/// call fails: long enough for a large prompt's first token.
pub const SILENCE: Duration = Duration::from_secs(180);

/// How long a run that's ending, or that was interrupted, gets to say its
/// `result` and exit before it's killed.
pub const WIND_DOWN: Duration = Duration::from_secs(5);

/// What a reply's call gets when the turn's next request has no result
/// for it.
pub const NOT_RUN: &str = "smelt didn't run this tool call.";

/// What a tool call gets that isn't one of the reply's calls.
pub const NOT_THE_REPLYS: &str = "smelt refused this tool call: it isn't one of your reply's calls.";

#[cfg(test)]
pub static SILENCE_FOR_TESTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn silence() -> Duration {
    #[cfg(test)]
    {
        let ms = SILENCE_FOR_TESTS.load(std::sync::atomic::Ordering::SeqCst);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    SILENCE
}

/// A turn's run, if it has one: held by the turn for as long as it runs.
#[derive(Default)]
pub struct TurnCli {
    run: Option<Run>,
}

impl Drop for TurnCli {
    fn drop(&mut self) {
        if let Some(run) = self.run.take() {
            run.end_in_background();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    Streaming,
    Parked,
    Idle,
}

/// One of the reply's tool calls the CLI will make.
struct Expected {
    id: String,
    name: String,
    input: Value,
}

struct Run {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
    stderr: Arc<Mutex<String>>,
    bridge: RunBridge,
    state: State,
    /// The calls of the reply last returned, while Parked.
    expected: Vec<Expected>,
    /// Calls that arrived before smelt had their result, by tool-use id.
    arrived: HashMap<String, ParkedCall>,
    /// Calls that arrived with no tool-use id.
    anonymous: Vec<ParkedCall>,
    /// Results for calls that haven't arrived yet, by tool-use id.
    answers: HashMap<String, ToolReply>,
    synced: Synced,
    rate: Option<RateLimit>,
    /// The prompt and `--mcp-config` files, removed when the run goes.
    _files: RunFiles,
    /// False once the run can't be trusted with another call.
    healthy: bool,
    interrupts: u64,
    /// The calls smelt has answered.
    answered: std::collections::HashSet<String>,
    /// The tool calls the reply streaming now has started.
    streaming_ids: std::collections::HashSet<String>,
    /// Whether the last reply's results are in `answers`, until the CLI's
    /// next API call starts.
    delivered: bool,
    /// Whether it was given smelt's tools (an `--mcp-config`).
    has_tools: bool,
    /// The reply cap it was launched with.
    max_tokens: u32,
    /// Whether the CLI has said anything since the current call began.
    heard: bool,
    /// Whether its stdout ended or its input broke: the process is gone.
    gone: bool,
}

/// One model call through the turn's run: starts one if the turn has
/// none (or its history went another way), sends what's new, and reads
/// the reply. The reply's usage goes to `meter`, and that of any API call
/// the CLI made on its own after a cut-off to `extra` (S6).
pub async fn call(
    conversation_id: i64,
    turn_model: &crate::providers::TurnModel,
    request: &CreateMessageRequest,
    on_delta: &mut (dyn FnMut(&str) + Send),
    meter: &UsageMeter,
    extra: &UsageMeter,
    cli: &mut TurnCli,
) -> Result<StreamedTurn, String> {
    // A process that died since the last call (an OOM kill while smelt ran
    // a tool) is replaced, as the state table's Dead row says (review 1 M1).
    if let Some(run) = cli.run.as_mut()
        && !run.alive()
        && let Some(dead) = cli.run.take()
    {
        tracing::warn!(conversation_id, "the turn's Claude Code process had exited: starting a new one from smelt's history");
        dead.end_in_background();
    }
    // The CLI takes the reply's cap at launch (CLAUDE_CODE_MAX_OUTPUT_TOKENS):
    // a call with another one needs a run of its own (review 1 L3).
    if let Some(run) = cli.run.as_ref()
        && run.max_tokens != request.max_tokens
        && let Some(old) = cli.run.take()
    {
        tracing::info!(conversation_id, from = old.max_tokens, to = request.max_tokens, "the reply's cap changed: starting a new Claude Code run");
        old.end_in_background();
    }
    let tail = cli.run.as_ref().and_then(|run| tail_after(&run.synced, &request.messages)).map(<[_]>::to_vec);
    if tail.is_none()
        && let Some(old) = cli.run.take()
    {
        tracing::info!(conversation_id, "the conversation went on without its Claude Code run: starting a new one from smelt's history");
        old.end_in_background();
    }
    if let (Some(run), Some(tail)) = (cli.run.as_mut(), &tail)
        && let Err(e) = run.send_tail(tail).await
    {
        let Some(mut run) = cli.run.take() else {
            return Err(e);
        };
        // Its input closed under it: it died as smelt wrote (M1).
        let died = run.exited();
        run.end_in_background();
        if !died {
            return Err(e);
        }
    }
    let replied = read_from_run(conversation_id, turn_model, request, on_delta, meter, extra, cli).await;
    match replied {
        // It died before it said anything to this call (M1, the race the
        // check above can lose): once more, on a new run.
        Err(_) if cli.run.as_mut().is_some_and(|run| run.exited() && !run.heard) => {
            if let Some(dead) = cli.run.take() {
                tracing::warn!(conversation_id, "the turn's Claude Code process exited before it answered: starting a new one from smelt's history");
                dead.end_in_background();
            }
            read_from_run(conversation_id, turn_model, request, on_delta, meter, extra, cli).await
        }
        replied => replied,
    }
}

/// `call`'s spawn (when the turn has no run) and its read of the reply.
async fn read_from_run(
    conversation_id: i64,
    turn_model: &crate::providers::TurnModel,
    request: &CreateMessageRequest,
    on_delta: &mut (dyn FnMut(&str) + Send),
    meter: &UsageMeter,
    extra: &UsageMeter,
    cli: &mut TurnCli,
) -> Result<StreamedTurn, String> {
    if cli.run.is_none() {
        let mut run = Run::spawn(conversation_id, turn_model, request).await?;
        if request.messages.iter().any(|m| m.role == "assistant") {
            tracing::info!(conversation_id, messages = request.messages.len(), "a new Claude Code run starts from smelt's history as text");
        }
        if let Err(e) = run.write_user(&rehydrate(&request.messages, turn_model.context_window)).await {
            run.end_in_background();
            return Err(e);
        }
        cli.run = Some(run);
    }
    let Some(run) = cli.run.as_mut() else {
        return Err("Claude Code's run is gone.".to_string());
    };
    run.synced = synced(&request.messages);
    run.heard = false;
    let replied = run.read_reply(on_delta, meter, extra).await;
    // A run that died before it said anything is kept for `call` to see.
    if (replied.is_err() || !run.healthy) && !(run.exited() && !run.heard) {
        if let Some(run) = cli.run.take() {
            run.end_in_background();
        }
    }
    replied
}

impl Run {
    async fn spawn(conversation_id: i64, turn_model: &crate::providers::TurnModel, request: &CreateMessageRequest) -> Result<Self, String> {
        let env = super::cli_env(|name| std::env::var_os(name));
        let value = |name: &str| env.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
        let config_dir = value("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .ok_or_else(|| format!("Claude Code has no config directory: set {}, or HOME, in the server's environment.", super::CONFIG_DIR_VAR))?;
        let work = config_dir.join("work").join(conversation_id.to_string());
        let prompts = config_dir.join("prompts");
        for dir in [&config_dir, &work, &prompts] {
            super::private_dir(dir).map_err(|e| format!("Couldn't make Claude Code's directory {} private: {e}", dir.display()))?;
        }
        let binary = super::resolve_binary(&turn_model.endpoint.base_url, value("PATH").as_deref())?;
        let prompt_file = prompts.join(format!("{conversation_id}-{}.md", super::bridge::random_hex(8)));
        let config_file = (!request.tools.is_empty()).then(|| prompt_file.with_extension("mcp.json"));
        // Made before either file is written: any return from here on, or a
        // drop of the run, removes them (review 1 L6).
        let files = RunFiles(std::iter::once(prompt_file.clone()).chain(config_file.clone()).collect());
        super::process::write_private(&prompt_file, request.system.as_deref().unwrap_or_default().as_bytes())
            .map_err(|e| format!("Couldn't write the system prompt for Claude Code: {e}"))?;
        let run_bridge = bridge().await?.register(&request.tools);
        if let Some(config_file) = &config_file {
            super::process::write_private(config_file, run_bridge.mcp_config().to_string().as_bytes())
                .map_err(|e| format!("Couldn't write Claude Code's MCP config: {e}"))?;
        }
        let launch = Launch {
            model: &turn_model.model,
            effort: turn_model.effort,
            system_prompt_file: &prompt_file,
            mcp_config_file: config_file.as_deref(),
            session: SessionArg::Ephemeral,
        };
        let env = turn_env(env, request.max_tokens);
        let spawned = tokio::process::Command::new(&binary)
            .args(cli_args(&launch))
            .env_clear()
            .envs(env.iter().map(|(name, value)| (name, value)))
            .current_dir(&work)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => return Err(format!("Couldn't run Claude Code ({}): {e}", binary.display())),
        };
        let (Some(stdin), Some(stdout), Some(stderr_pipe)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
            return Err("Claude Code's pipes weren't made.".to_string());
        };
        let stderr = Arc::new(Mutex::new(String::new()));
        tokio::spawn(keep_tail(stderr_pipe, stderr.clone()));
        Ok(Self {
            child,
            stdin: Some(stdin),
            lines: BufReader::new(stdout).lines(),
            stderr,
            bridge: run_bridge,
            state: State::Streaming,
            expected: Vec::new(),
            arrived: HashMap::new(),
            anonymous: Vec::new(),
            answers: HashMap::new(),
            synced: synced(&[]),
            rate: None,
            _files: files,
            healthy: true,
            interrupts: 0,
            heard: false,
            gone: false,
            max_tokens: request.max_tokens,
            has_tools: !request.tools.is_empty(),
            streaming_ids: std::collections::HashSet::new(),
            answered: std::collections::HashSet::new(),
            delivered: false,
        })
    }

    async fn write_line(&mut self, line: &Value) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("Claude Code's input is closed.")?;
        let mut bytes = line.to_string().into_bytes();
        bytes.push(b'\n');
        let written = match stdin.write_all(&bytes).await {
            Ok(()) => stdin.flush().await,
            Err(e) => Err(e),
        };
        written.map_err(|e| {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                self.gone = true;
            }
            format!("Couldn't write to Claude Code: {e}")
        })
    }

    /// Whether the process is still running.
    fn alive(&mut self) -> bool {
        !self.gone && matches!(self.child.try_wait(), Ok(None))
    }

    /// Whether the process is known to be gone.
    fn exited(&mut self) -> bool {
        self.gone || matches!(self.child.try_wait(), Ok(Some(_)))
    }

    async fn write_user(&mut self, text: &str) -> Result<(), String> {
        self.write_line(&serde_json::json!({"type": "user", "message": {"role": "user", "content": text}})).await?;
        self.state = State::Streaming;
        Ok(())
    }

    async fn interrupt(&mut self) -> Result<(), String> {
        self.interrupts += 1;
        let id = format!("smelt-interrupt-{}", self.interrupts);
        self.write_line(&serde_json::json!({"type": "control_request", "request_id": id, "request": {"subtype": "interrupt"}})).await
    }

    /// Sends what the turn added since the last call: the reply's results
    /// to its parked calls, or a user message.
    async fn send_tail(&mut self, tail: &[AnthropicMessage]) -> Result<(), String> {
        match self.state {
            State::Streaming => Err("Claude Code was still replying when smelt called it again.".to_string()),
            State::Idle => {
                let names = call_names(tail);
                let text = tail.iter().map(|m| blocks_text(&m.content, &names)).collect::<Vec<_>>().join("\n\n");
                self.write_user(&text).await
            }
            State::Parked => {
                let mut replies: HashMap<String, ToolReply> = HashMap::new();
                let mut others = Vec::new();
                for block in tail.iter().flat_map(|m| &m.content) {
                    match block {
                        ContentBlock::ToolResult { tool_use_id, content, is_error }
                            if self.expected.iter().any(|e| &e.id == tool_use_id) =>
                        {
                            let reply = ToolReply { text: vec![content.clone()], is_error: is_error.unwrap_or(false) };
                            replies.insert(tool_use_id.clone(), reply);
                        }
                        ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                            others.push(tool_result_text(tool_use_id, None, content, is_error.unwrap_or(false)));
                        }
                        other => others.push(blocks_text(std::slice::from_ref(other), &HashMap::new())),
                    }
                }
                others.retain(|text| !text.trim().is_empty());
                for expected in &self.expected {
                    replies
                        .entry(expected.id.clone())
                        .or_insert_with(|| ToolReply { text: vec![NOT_RUN.to_string()], is_error: true });
                }
                if let Some(last) = self.expected.last().and_then(|e| replies.get_mut(&e.id)) {
                    last.text.extend(others);
                }
                self.answers = replies;
                self.delivered = true;
                for parked in std::mem::take(&mut self.anonymous) {
                    self.take_call(parked);
                }
                let arrived: Vec<String> = self.arrived.keys().cloned().collect();
                for id in arrived {
                    if let Some(parked) = self.arrived.remove(&id) {
                        self.take_call(parked);
                    }
                }
                self.state = State::Streaming;
                Ok(())
            }
        }
    }

    /// A call from the CLI: answered now if smelt has its result, else
    /// kept until it does. One without a tool-use id is matched to a call
    /// of the reply by name and input, first come first served.
    fn take_call(&mut self, parked: ParkedCall) {
        let id = parked.tool_use_id.clone().or_else(|| {
            self.expected
                .iter()
                .find(|e| e.name == parked.name && e.input == parked.arguments && self.answers.contains_key(&e.id))
                .map(|e| e.id.clone())
        });
        let refuse = |parked: ParkedCall| {
            let _ = parked.reply.send(ToolReply { text: vec![NOT_THE_REPLYS.to_string()], is_error: true });
        };
        match id {
            Some(id) => match self.answers.remove(&id) {
                Some(reply) => {
                    let _ = parked.reply.send(reply);
                    self.answered.insert(id);
                }
                // Not answered yet: the CLI makes a reply's first call
                // before its `message_stop` (S2), and smelt runs it first.
                None if self.expected.iter().any(|e| e.id == id) || self.streaming_ids.contains(&id) => {
                    self.arrived.insert(id, parked);
                }
                // Not one of the reply's calls: refused at once (review 1 L1).
                None => refuse(parked),
            },
            // No id, and its input may not have streamed yet: it waits for
            // the reply's results, and is refused once they're in.
            None if !self.delivered => self.anonymous.push(parked),
            None => refuse(parked),
        }
    }

    fn stderr_tail(&self) -> String {
        self.stderr.lock().map(|tail| tail.trim().to_string()).unwrap_or_default()
    }

    /// The next line from the CLI, the bridge's calls taken as they come.
    async fn next_event(&mut self) -> Result<Value, String> {
        loop {
            tokio::select! {
                Some(parked) = self.bridge.calls.recv() => self.take_call(parked),
                line = tokio::time::timeout(silence(), self.lines.next_line()) => {
                    let line = match line {
                        Err(_) => {
                            self.healthy = false;
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
                            let line = super::errors::scrubbed_line(&tail);
                            let said = if line.is_empty() { String::new() } else { format!(" It said: {line}") };
                            return Err(format!("Claude Code exited ({code}) before it finished.{said}"));
                        }
                    };
                    // A line that isn't a JSON object isn't the CLI's protocol.
                    if let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(&line) {
                        self.heard = true;
                        return Ok(value);
                    }
                }
            }
        }
    }

    /// Reads one API call's reply.
    async fn read_reply(&mut self, on_delta: &mut (dyn FnMut(&str) + Send), meter: &UsageMeter, extra: &UsageMeter) -> Result<StreamedTurn, String> {
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
                        Some("message_start") => self.delivered = false,
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
                    if let Some(id) = self.answered_by_the_cli(&event) {
                        self.healthy = false;
                        return Err(format!("Claude Code answered tool call {id} itself, so its context and smelt's no longer agree; smelt stopped the call. Send another message to carry on from smelt's record."));
                    }
                }
                Some("result") => {
                    self.state = State::Idle;
                    self.healthy = false;
                    return Err(result_error(&event, self.rate.as_ref())
                        .unwrap_or_else(|| "Claude Code ended the reply before it finished.".to_string()));
                }
                _ => self.note(&event),
            }
        }
    }

    /// The id of a call of the parked reply that the CLI's echoed
    /// `tool_result` answers though smelt never did (review 1 L2).
    fn answered_by_the_cli(&self, event: &Value) -> Option<String> {
        event["message"]["content"].as_array()?.iter().find_map(|block| {
            let id = block["tool_use_id"].as_str()?;
            let ours = self.expected.iter().any(|e| e.id == id);
            (block["type"] == "tool_result" && ours && !self.answered.contains(id)).then(|| id.to_string())
        })
    }

    /// What smelt notes of a line that isn't the reply's.
    fn note(&mut self, event: &Value) {
        match (event["type"].as_str(), event["subtype"].as_str()) {
            (Some("rate_limit_event"), _) => self.rate = Some(RateLimit::from_event(event)),
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
    async fn finish(&mut self, blocks: Blocks, stop_reason: String, meter: &UsageMeter, extra: &UsageMeter) -> Result<StreamedTurn, String> {
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
            return Ok(turn);
        }
        if crate::anthropic::stream::is_cut_off(&turn.stop_reason) {
            // The CLI would call again on its own (S6): stop it now.
            if self.interrupt().await.is_err() {
                self.healthy = false;
            }
        }
        self.await_result(extra).await;
        Ok(turn)
    }

    /// Reads to the run's `result`, within `WIND_DOWN`: an API call the
    /// CLI made on its own meanwhile is metered on `extra` and otherwise
    /// dropped. A run that doesn't get there isn't used again.
    async fn await_result(&mut self, extra: &UsageMeter) {
        let waited = tokio::time::timeout(WIND_DOWN, async {
            loop {
                let event = self.next_event().await?;
                match event["type"].as_str() {
                    Some("result") => return Ok::<_, String>(()),
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

    /// Ends the run in a spawned task: interrupt if it's mid-reply, then
    /// close its input, wait, and kill what's left of its process group.
    fn end_in_background(mut self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            if self.state != State::Idle && self.interrupt().await.is_ok() {
                let _ = tokio::time::timeout(WIND_DOWN, async {
                    while let Ok(event) = self.next_event().await {
                        if event["type"] == "result" {
                            break;
                        }
                    }
                })
                .await;
            }
            // Answered with an error only now, after the interrupt: the CLI
            // then records each as stopped rather than calling again.
            self.arrived.clear();
            self.anonymous.clear();
            self.stdin = None;
            if tokio::time::timeout(WIND_DOWN, self.child.wait()).await.is_err() {
                if let Some(group) = self.child.id().and_then(|id| i32::try_from(id).ok()) {
                    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(group), nix::sys::signal::Signal::SIGKILL);
                }
                let _ = self.child.wait().await;
            }
        });
    }
}

/// A run's files: its system prompt and its `--mcp-config` (which holds
/// the bridge's token), removed when the run is dropped, however it ends
/// (review 1 L6). The CLI reads both only as it starts.
struct RunFiles(Vec<PathBuf>);

impl Drop for RunFiles {
    fn drop(&mut self) {
        for file in &self.0 {
            let _ = std::fs::remove_file(file);
        }
    }
}

/// One `stream_event`'s API event, applied to the reply as
/// `anthropic::stream` applies an SSE event.
fn apply(blocks: &mut Blocks, stop_reason: &mut String, event: &Value, on_delta: &mut (dyn FnMut(&str) + Send), meter: &UsageMeter) -> Result<(), String> {
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

/// Whether an `init` event says the CLI connected to smelt's bridge
/// (review 1 L7).
fn bridge_connected(init: &Value) -> bool {
    init["mcp_servers"]
        .as_array()
        .is_some_and(|servers| servers.iter().any(|s| s["name"] == super::bridge::SERVER_NAME && s["status"] == "connected"))
}

/// Keeps the last `errors::MAX_QUOTED` bytes of the CLI's stderr.
async fn keep_tail(mut stderr: tokio::process::ChildStderr, tail: Arc<Mutex<String>>) {
    let mut buffer = [0u8; 4096];
    while let Ok(n) = stderr.read(&mut buffer).await {
        if n == 0 {
            break;
        }
        let Ok(mut kept) = tail.lock() else { break };
        kept.push_str(&String::from_utf8_lossy(&buffer[..n]));
        let excess = kept.len().saturating_sub(super::errors::MAX_QUOTED);
        if excess > 0 {
            let cut = (excess..=kept.len()).find(|&i| kept.is_char_boundary(i)).unwrap_or(kept.len());
            kept.drain(..cut);
        }
    }
}
