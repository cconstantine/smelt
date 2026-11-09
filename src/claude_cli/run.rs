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
//! - call, with a larger reply cap than the run's launch (the CLI reads
//!   it once): end the run, then as None (review 1 L3). A smaller one keeps
//!   the run (review 2 M3).
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
//! - the turn returned (`TurnCli::close`: it finished, failed, or asked)
//!   while Parked, with a reply for each of the reply's calls (its held
//!   result, or a question's placeholder, `close::replies`) → **Closing**:
//!   answer each call, await the CLI's echo of each (or its next API
//!   call starting), then interrupt, close stdin, wait, kill; within
//!   `CLOSE_WAIT`, past which it interrupts anyway (SME-444).
//! - `TurnCli` dropped otherwise (a Stop, or a call missing its result):
//!   Streaming or Parked → interrupt (the CLI then records each parked
//!   call as stopped, the planner's P3), then close stdin, wait, kill;
//!   Idle → close stdin, wait, kill. In a spawned task, so a Stop never
//!   waits for it, and so does Closing.

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
use super::render::{Synced, blocks_text, call_names, rehydrate, synced, tail_after, tool_result_text, without_delivered};
use crate::anthropic::stream::interpret::{StreamOutcome, block_index, interpret_stream_event};
use crate::anthropic::stream::{Blocks, PartialBlock, StreamedTurn, UsageMeter};
use crate::anthropic::{AnthropicMessage, ContentBlock, CreateMessageRequest};

/// The longest the CLI may say nothing while smelt waits on it, before the
/// call fails: long enough for a large prompt's first token.
pub const SILENCE: Duration = Duration::from_secs(180);

/// How long a run that's ending, or that was interrupted, gets to say its
/// `result` and exit before it's killed.
pub const WIND_DOWN: Duration = Duration::from_secs(5);

/// Taken by each process as it starts and held until it has started
/// (its first line), so no two write the shared config dir's `.claude.json`
/// at once (SME-399's L11).
static LAUNCH: std::sync::LazyLock<Arc<tokio::sync::Mutex<()>>> = std::sync::LazyLock::new(Arc::default);

/// The longest a start holds `LAUNCH`, for a CLI that says nothing.
pub const LAUNCH_HOLD: Duration = Duration::from_secs(10);

/// The longest a write to the CLI's input may take.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

fn launch_hold() -> Duration {
    #[cfg(test)]
    {
        let ms = LAUNCH_HOLD_FOR_TESTS.load(std::sync::atomic::Ordering::SeqCst);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    LAUNCH_HOLD
}

pub(super) fn write_timeout() -> Duration {
    #[cfg(test)]
    {
        let ms = WRITE_TIMEOUT_FOR_TESTS.load(std::sync::atomic::Ordering::SeqCst);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    WRITE_TIMEOUT
}

/// A start's hold on `LAUNCH`, let go at its first line (`release`) or,
/// from a timer of its own, at `launch_hold()`, whatever the run is doing
/// meanwhile (PR #285 review 1 L18).
pub(super) struct Launching(Arc<Mutex<Option<tokio::sync::OwnedMutexGuard<()>>>>);

impl Launching {
    /// Takes the start lock: a one-shot call's start, too (SME-431).
    pub(super) async fn take() -> Self {
        Self::hold(LAUNCH.clone().lock_owned().await)
    }

    fn hold(guard: tokio::sync::OwnedMutexGuard<()>) -> Self {
        let held = Arc::new(Mutex::new(Some(guard)));
        let timer = Arc::downgrade(&held);
        tokio::spawn(async move {
            tokio::time::sleep(launch_hold()).await;
            if let Some(held) = timer.upgrade() {
                release(&held);
            }
        });
        Self(held)
    }

    pub(super) fn release(&self) {
        release(&self.0);
    }
}

fn release(held: &Mutex<Option<tokio::sync::OwnedMutexGuard<()>>>) {
    // A guard taken out whole: nothing is left half-done.
    if let Ok(mut held) = held.lock() {
        held.take();
    }
}

#[cfg(test)]
pub static LAUNCH_HOLD_FOR_TESTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
pub static WRITE_TIMEOUT_FOR_TESTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What a reply's call gets when the turn's next request has no result
/// for it.
pub const NOT_RUN: &str = "smelt didn't run this tool call.";

/// What a tool call gets that isn't one of the reply's calls.
pub const NOT_THE_REPLYS: &str = "smelt refused this tool call: it isn't one of your reply's calls.";

/// What an `ask_user` call is answered with as its turn ends (SME-444): a
/// success, so the model doesn't read the run's close as a refusal.
pub const ASKED_USER: &str = "smelt is showing your questions to the user. Their answer arrives as your next message: end your turn now, without another tool call.";

/// What an `ask_parent` call is answered with as its turn ends (SME-444).
pub const ASKED_PARENT: &str = "smelt has sent your question to your parent. Its answer arrives as your next message: end your turn now, without another tool call.";

#[cfg(test)]
pub static SILENCE_FOR_TESTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) fn silence() -> Duration {
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
    /// How the last call that failed left its session.
    last_failure: Option<Failure>,
    /// The last call's failure, when it was an overflow (SME-431).
    overflow: Option<crate::anthropic::overflow::Overflow>,
    /// The turn's last tool results, saved but not yet sent: the next
    /// call sends them, or the close answers the parked calls with them
    /// (SME-444).
    results: Vec<ContentBlock>,
    /// The question those results' turn asked, once recorded: the only
    /// call the close answers with a placeholder (PR #295 review 1 M1).
    asked: Option<String>,
}

impl TurnCli {
    /// The overflow the last call failed with, if it did: SME-73's
    /// recovery compacts on it.
    pub fn take_overflow(&mut self) -> Option<crate::anthropic::overflow::Overflow> {
        self.overflow.take()
    }

    /// Holds the results the turn just saved for its reply's calls, and
    /// the id of the question it asked, replacing any held before: the
    /// close answers with them if the turn returns before its next call
    /// (SME-444).
    pub fn hold_results(&mut self, results: &[ContentBlock], asked: Option<&str>) {
        self.results = results.to_vec();
        self.asked = asked.map(str::to_string);
    }

    /// The turn returned, Ok or Err: a run Parked on calls smelt has a
    /// reply for each of is closed (answered, echoed, then interrupted),
    /// and any other ends as a drop ends it. Only a return reaches this:
    /// a Stop drops the turn, so its calls still read as stopped.
    pub fn close(&mut self) {
        let Some(run) = self.run.take() else { return };
        let closing = if run.state == State::Parked { close::replies(&run.expected, &self.results, self.asked.as_deref()) } else { None };
        match closing {
            Some(closing) => run.ending(Some(closing)),
            None => run.end_in_background(),
        }
    }
}

/// How a failed call left the CLI's session (PR #285 review 1 M2).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Failure {
    /// As it was: the API refused before the reply began (the limit, a
    /// 429, 529 or 5xx, a silence), so the record still holds.
    Unchanged,
    /// The CLI has no such session to resume ("No conversation found with
    /// session ID", the capture `missing`): a new one replaces it.
    SessionMissing,
    /// Possibly not as smelt's history has it (the CLI answered a call
    /// itself, compacted, or failed mid-reply), or unknown: forgotten.
    #[default]
    Diverged,
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
pub(in crate::claude_cli) struct Expected {
    pub(in crate::claude_cli) id: String,
    /// smelt's name for its tool (`read::finish` maps the CLI's).
    pub(in crate::claude_cli) name: String,
    pub(in crate::claude_cli) input: Value,
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
    /// The start lock, until its first line or the deadline (L11).
    launching: Launching,
    /// The CLI session it's in.
    session_id: String,
    /// Whether it resumed a session from an earlier turn.
    resumed: bool,
    /// Whether this call's reply has begun (a `message_start`).
    reply_started: bool,
    /// Calls with an id the streaming reply hasn't shown yet (L12).
    held: Vec<ParkedCall>,
    /// Whether it was given smelt's tools (an `--mcp-config`).
    has_tools: bool,
    /// The tool names the CLI's `init` listed as callable, in the CLI's own
    /// `mcp__smelt__…` forms (SME-447): an echoed `tool_result` for a name
    /// not in it can only be the CLI's local unknown-name refusal, judged
    /// by name and never by wording. `None` till one is seen — an
    /// uncheckable echo stays today's divergence.
    cli_tools: Option<std::collections::HashSet<String>>,
    /// The conversation it's a turn of.
    conversation_id: i64,
    /// How this call failed, if it did.
    failure: Failure,
    /// The reply cap it was launched with.
    max_tokens: u32,
    /// Whether the CLI has said anything since the current call began.
    heard: bool,
    /// Whether its stdout ended or its input broke: the process is gone.
    gone: bool,
    /// The model it runs.
    model: String,
    /// The window and reply cap its last `result` reported for the model,
    /// until `call` saves them (SME-431).
    reported: Option<(i32, Option<i32>)>,
    /// Whether it has saved them.
    learned: bool,
    /// The overflow its last `result` said, if it said one.
    overflow: Option<crate::anthropic::overflow::Overflow>,
}

/// One model call through the turn's run: starts one if the turn has
/// none (or its history went another way), sends what's new, and reads
/// the reply. The reply's usage goes to `meter`, and that of any API call
/// the CLI made on its own after a cut-off to `extra` (S6).
pub async fn call(
    pool: &sqlx::PgPool,
    conversation_id: i64,
    turn_model: &crate::providers::TurnModel,
    request: &CreateMessageRequest,
    on_delta: &mut (dyn FnMut(&str) + Send),
    meter: &UsageMeter,
    extra: &UsageMeter,
    cli: &mut TurnCli,
) -> Result<StreamedTurn, String> {
    // The record goes before anything is sent, and comes back only after
    // a call that went well: a call that doesn't finish (a Stop drops it
    // mid-reply) leaves no record the next turn would resume past, sending
    // again what the session already has (PR #285 review 1 M1).
    let recorded = crate::db::get_claude_cli_session(pool, conversation_id).await.unwrap_or_else(|e| {
        tracing::warn!(conversation_id, "couldn't read the conversation's Claude Code session: {e}");
        None
    });
    if recorded.is_some()
        && let Err(e) = crate::db::delete_claude_cli_session(pool, conversation_id).await
    {
        tracing::warn!(conversation_id, "couldn't set the conversation's Claude Code session aside: {e}");
    }
    let call = Call { conversation_id, turn_model, request, recorded };
    cli.last_failure = None;
    cli.overflow = None;
    // This call sends the held results the normal way (SME-444).
    cli.results.clear();
    cli.asked = None;
    let replied = call_run(&call, on_delta, meter, extra, cli).await;
    // The session the next turn resumes: this run's, synced to this call,
    // after a call that went well; after any other, none, so the next turn
    // starts over from smelt's record.
    let keep = match (&replied, cli.run.as_ref()) {
        (Ok(_), Some(run)) if run.healthy => Some(crate::db::ClaudeCliSession {
            provider_id: turn_model.provider_id,
            session_id: run.session_id.clone(),
            synced_count: i32::try_from(run.synced.len).unwrap_or(i32::MAX),
            synced_hash: run.synced.hash.clone(),
        }),
        // A failure that left the session as it was keeps its record: the
        // next turn resumes it and is sent what's new since (review 1 M2).
        (Err(_), _) if cli.last_failure == Some(Failure::Unchanged) => call.recorded.clone(),
        _ => None,
    };
    let kept = match &keep {
        Some(session) => crate::db::set_claude_cli_session(pool, conversation_id, session).await,
        None => Ok(()),
    };
    if let Err(e) = kept {
        tracing::warn!(conversation_id, "couldn't record the conversation's Claude Code session: {e}");
    }
    // The window the CLI reported, the model row's from now on: the
    // compaction trigger's (SME-431). Once a run.
    if let Some(run) = cli.run.as_mut()
        && let Some((window, max_output)) = run.reported.take()
        && !std::mem::replace(&mut run.learned, true)
        && let Err(e) = crate::providers::learn_claude_cli_window(pool, turn_model.provider_id, &turn_model.model, window, max_output).await
    {
        tracing::warn!(conversation_id, "couldn't save the window Claude Code reported: {e}");
    }
    replied
}

/// One call's fixed inputs.
struct Call<'a> {
    conversation_id: i64,
    turn_model: &'a crate::providers::TurnModel,
    request: &'a CreateMessageRequest,
    /// The conversation's session as recorded when the call began.
    recorded: Option<crate::db::ClaudeCliSession>,
}

/// `call` without the session row's upkeep.
async fn call_run(
    call: &Call<'_>,
    on_delta: &mut (dyn FnMut(&str) + Send),
    meter: &UsageMeter,
    extra: &UsageMeter,
    cli: &mut TurnCli,
) -> Result<StreamedTurn, String> {
    let (conversation_id, request) = (call.conversation_id, call.request);
    // A compaction replaced the history this run's session holds
    // (SME-431): the run ends, and the call starts a new session from the
    // summary (the state table's B2 row).
    if super::session::take_reset(conversation_id)
        && let Some(old) = cli.run.take()
    {
        tracing::info!(conversation_id, "compacted: ending the turn's Claude Code run");
        old.end_in_background();
    }
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
    // a call that needs a larger one needs a run of its own (review 1 L3).
    // A smaller one keeps the run, so a turn near the window, whose cap
    // shrinks at every call, isn't re-sent uncached each time: smelt's own
    // room check before the call governs the reply (review 2 M3).
    if let Some(run) = cli.run.as_ref()
        && request.max_tokens > run.max_tokens
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
        && let Err(e) = run.send_tail(tail, &request.messages).await
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
    let replied = read_from_run(call, on_delta, meter, extra, cli).await;
    match replied {
        // It died before it said anything to this call (M1, the race the
        // check above can lose): once more, on a new run.
        Err(_) if cli.run.as_mut().is_some_and(|run| run.exited() && !run.heard) => {
            if let Some(dead) = cli.run.take() {
                tracing::warn!(conversation_id, "the turn's Claude Code process exited before it answered: starting a new one from smelt's history");
                dead.end_in_background();
            }
            read_from_run(call, on_delta, meter, extra, cli).await
        }
        replied => replied,
    }
}

/// Records how the call failed.
fn cli_failure(last: &mut Option<Failure>, failure: Failure) {
    *last = Some(failure);
}

/// `call`'s start of a run, when the turn has none, and its read of the
/// reply. A resumed session that fails before its reply begins (gone with
/// a restart's config dir, say) is replaced once by a new one.
async fn read_from_run(
    call: &Call<'_>,
    on_delta: &mut (dyn FnMut(&str) + Send),
    meter: &UsageMeter,
    extra: &UsageMeter,
    cli: &mut TurnCli,
) -> Result<StreamedTurn, String> {
    if cli.run.is_none() {
        cli.run = Some(start(call, true).await?);
    }
    let replied = read_once(call, on_delta, meter, extra, cli).await;
    if replied.is_err()
        && cli.last_failure == Some(Failure::SessionMissing)
        && let Some(run) = cli.run.take()
    {
        tracing::warn!(conversation_id = call.conversation_id, "the conversation's Claude Code session didn't resume: starting a new one from smelt's history");
        run.end_in_background();
        cli.run = Some(start(call, false).await?);
        return read_once(call, on_delta, meter, extra, cli).await;
    }
    replied
}

/// A new run for the turn: resuming the conversation's session when
/// `resume` and its record (`claude_cli_sessions`) still matches smelt's
/// history, sent what's new; else a new session, sent the history as text.
async fn start(call: &Call<'_>, resume: bool) -> Result<Run, String> {
    let (conversation_id, turn_model, request) = (call.conversation_id, call.turn_model, call.request);
    let recorded = if resume { call.recorded.clone() } else { None };
    let resumable = recorded.filter(|s| s.provider_id == turn_model.provider_id).and_then(|s| {
        let synced = Synced { len: usize::try_from(s.synced_count).ok()?, hash: s.synced_hash };
        tail_after(&synced, &request.messages).map(|tail| (s.session_id, tail.to_vec()))
    });
    let (mut run, first) = match resumable {
        Some((session_id, tail)) => {
            // Never two processes in one session: the conversation's last one
            // (L3's restart, or the last turn's) may still be ending
            // (PR #285 review 1 L17). A closing one writes what it delivered
            // as it ends, so that's taken after (SME-444).
            ended(conversation_id).await;
            let tail = without_delivered(&tail, &super::session::take_delivered(conversation_id));
            let names = call_names(&request.messages);
            let text = tail.iter().map(|m| blocks_text(&m.content, &names)).collect::<Vec<_>>().join("\n\n");
            let mut run = Run::spawn(conversation_id, turn_model, request, SessionArg::Resume(&session_id)).await?;
            run.resumed = true;
            run.session_id = session_id;
            (run, text)
        }
        None => {
            if request.messages.iter().any(|m| m.role == "assistant") {
                tracing::info!(conversation_id, messages = request.messages.len(), "a new Claude Code session starts from smelt's history as text");
            }
            // A recorded session this one replaces goes now: it holds the
            // conversation's tool output and the CLI's account email
            // (PR #285 review 1 L20). Once its last process has ended, so
            // that one doesn't write it back (SME-431 L22). So do those a
            // compaction replaced (SME-431's reset).
            let mut replaced = super::session::take_retired(conversation_id);
            replaced.extend(call.recorded.iter().map(|old| old.session_id.clone()));
            if !replaced.is_empty() {
                ended(conversation_id).await;
            }
            for old in &replaced {
                super::files::forget_session(conversation_id, old);
            }
            // A new session has nothing delivered: it's sent the whole
            // history (SME-444).
            drop(super::session::take_delivered(conversation_id));
            let session_id = super::process::new_session_id();
            let mut run = Run::spawn(conversation_id, turn_model, request, SessionArg::New(&session_id)).await?;
            run.session_id = session_id;
            (run, rehydrate(&request.messages, turn_model.context_window))
        }
    };
    if let Err(e) = run.write_user(&first).await {
        run.end_in_background();
        return Err(e);
    }
    Ok(run)
}

/// Reads the turn's run's reply to this call.
async fn read_once(
    call: &Call<'_>,
    on_delta: &mut (dyn FnMut(&str) + Send),
    meter: &UsageMeter,
    extra: &UsageMeter,
    cli: &mut TurnCli,
) -> Result<StreamedTurn, String> {
    let Some(run) = cli.run.as_mut() else {
        return Err("Claude Code's run is gone.".to_string());
    };
    run.synced = synced(&call.request.messages);
    run.heard = false;
    run.reply_started = false;
    run.failure = Failure::Diverged;
    let replied = run.read_reply(on_delta, meter, extra).await;
    if replied.is_err() {
        cli_failure(&mut cli.last_failure, run.failure);
        cli.overflow = run.overflow.take();
    }
    // A run that died before it said anything is kept for `call` to see,
    // and so is one whose session is missing.
    let kept = (run.exited() && !run.heard) || run.failure == Failure::SessionMissing;
    if (replied.is_err() || !run.healthy) && !kept {
        if let Some(run) = cli.run.take() {
            run.end_in_background();
        }
    }
    replied
}

impl Run {
    async fn spawn(conversation_id: i64, turn_model: &crate::providers::TurnModel, request: &CreateMessageRequest, session: SessionArg<'_>) -> Result<Self, String> {
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
            session,
        };
        let env = turn_env(env, request.max_tokens);
        // One start at a time: every process shares the config dir, and
        // each writes its .claude.json as it starts (SME-399's L11). Held
        // until this one says its first line, or LAUNCH_HOLD passes.
        let launching = LAUNCH.clone().lock_owned().await;
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
            failure: Failure::Diverged,
            conversation_id,
            has_tools: !request.tools.is_empty(),
            cli_tools: None,
            held: Vec::new(),
            session_id: String::new(),
            resumed: false,
            reply_started: false,
            launching: Launching::hold(launching),
            streaming_ids: std::collections::HashSet::new(),
            answered: std::collections::HashSet::new(),
            delivered: false,
            model: turn_model.model.clone(),
            reported: None,
            learned: false,
            overflow: None,
        })
    }

    async fn write_line(&mut self, line: &Value) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("Claude Code's input is closed.")?;
        let mut bytes = line.to_string().into_bytes();
        bytes.push(b'\n');
        // Bounded: a CLI that never reads its input fails only its own call
        // (PR #285 review 1 L18).
        let writing = async {
            stdin.write_all(&bytes).await?;
            stdin.flush().await
        };
        let Ok(written) = tokio::time::timeout(write_timeout(), writing).await else {
            self.healthy = false;
            return Err(format!("Claude Code didn't take its input within {} seconds, so smelt stopped it.", write_timeout().as_secs()));
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

    fn stderr_tail(&self) -> String {
        self.stderr.lock().map(|tail| tail.trim().to_string()).unwrap_or_default()
    }

    /// Ends the run in a spawned task: interrupt if it's mid-reply, then
    /// close its input, wait, and kill what's left of its process group.
    fn end_in_background(self) {
        self.ending(None);
    }

    /// Ends the run in a spawned task, closing it first with `replies`
    /// when given (SME-444's Closing, `close::settle`): answered, echoed,
    /// then the same interrupt and wind-down as any run.
    fn ending(mut self, replies: Option<close::Close>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let conversation_id = self.conversation_id;
        let group = self.child.id().and_then(|id| i32::try_from(id).ok());
        let ending = runtime.spawn(async move {
            let interrupt = match replies {
                Some(replies) => self.settle(replies).await,
                None => self.state != State::Idle,
            };
            if interrupt && self.interrupt().await.is_ok() {
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
        let mut endings = endings();
        let mine = endings.entry(conversation_id).or_default();
        mine.retain(|ending| !ending.task.is_finished());
        mine.push(Ending { task: ending, group });
    }
}

/// A run being ended: its task, and its process group (the child's pid,
/// `process_group(0)`), for a wait that runs out to kill (SME-431 L21).
struct Ending {
    task: tokio::task::JoinHandle<()>,
    group: Option<i32>,
}

/// The tasks ending each conversation's runs (`end_in_background`).
fn endings() -> std::sync::MutexGuard<'static, HashMap<i64, Vec<Ending>>> {
    static ENDINGS: std::sync::LazyLock<Mutex<HashMap<i64, Vec<Ending>>>> = std::sync::LazyLock::new(Mutex::default);
    // Every change is one insert or take: nothing is left half-done.
    ENDINGS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The longest a resume waits for the conversation's last runs to end:
/// a close waits up to `CLOSE_WAIT` for its echoes (SME-444), an ending
/// interrupts within `WIND_DOWN`, waits as long again, then kills.
pub const ENDED_WAIT: Duration = Duration::from_secs(17);

#[cfg(test)]
pub static ENDED_WAIT_FOR_TESTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn ended_wait() -> Duration {
    #[cfg(test)]
    {
        let ms = ENDED_WAIT_FOR_TESTS.load(std::sync::atomic::Ordering::SeqCst);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    ENDED_WAIT
}

/// Whether the table holds an entry for `conversation_id`.
#[cfg(test)]
pub fn has_endings(conversation_id: i64) -> bool {
    endings().contains_key(&conversation_id)
}

/// Waits, within `ENDED_WAIT`, for every run of `conversation_id` that's
/// ending to be gone (PR #285 review 1 L17). One still going at the end
/// of the wait is killed, its task and its process group, so a session is
/// never in two processes (SME-431 L21): its interrupt alone can take
/// `WRITE_TIMEOUT` to fail on a CLI that doesn't read.
pub async fn ended(conversation_id: i64) {
    let ending = endings().remove(&conversation_id).unwrap_or_default();
    for Ending { mut task, group } in ending {
        if tokio::time::timeout(ended_wait(), &mut task).await.is_err() {
            tracing::warn!(conversation_id, "a Claude Code process of this conversation is still ending: killing it before its session goes on");
            // The task's drop of the run kills the child (`kill_on_drop`);
            // the group goes too, whatever it started.
            task.abort();
            if let Some(group) = group {
                let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(group), nix::sys::signal::Signal::SIGKILL);
            }
            let _ = task.await;
        }
    }
}

pub mod close;
mod deliver;
mod read;
pub(super) use read::apply;

/// A run's files: its system prompt and its `--mcp-config` (which holds
/// the bridge's token), removed when the run is dropped, however it ends
/// (review 1 L6). The CLI reads both only as it starts.
pub(super) struct RunFiles(pub(super) Vec<PathBuf>);

impl Drop for RunFiles {
    fn drop(&mut self) {
        for file in &self.0 {
            let _ = std::fs::remove_file(file);
        }
    }
}

/// Keeps the last `errors::MAX_QUOTED` bytes of the CLI's stderr.
pub(super) async fn keep_tail(mut stderr: tokio::process::ChildStderr, tail: Arc<Mutex<String>>) {
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
