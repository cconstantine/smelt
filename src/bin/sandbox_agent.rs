//! The sandbox agent: the sandbox image's ENTRYPOINT
//! (`docker/sandbox/Dockerfile`), so it starts with every sandbox pod
//! `src/sandbox.rs` creates, hosting N persistent named inner `bash` shells behind
//! one WebSocket server the main smelt process talks to. See
//! SME-9 for the full design — this
//! file implements the "Protocol" and the `sandbox_agent.rs` bullet in
//! "Which files" exactly: one agent process per pod, multiplexing every
//! terminal that pod hosts (`HashMap<terminal_id, Shell>`), each shell its
//! own process group (`.process_group(0)`) so terminating one never
//! touches the agent or any sibling terminal. Per-shell: the command
//! framing is unchanged, foreground `eval` (backgrounding it broke
//! `cd`/`export` persistence — see SME-9's "How"); `set -m` + `trap ':'
//! INT` at startup are both required, for different reasons, also covered
//! there; PID/process-group discovery for `send_signal` reads
//! `/proc/<bash_pid>/task/<bash_pid>/children` reactively, not eagerly.

// Shared with the server; the agent only needs `container_address`.
#[path = "../docker_net.rs"]
#[allow(dead_code)]
mod docker_net;
// The wire protocol, shared with the server (SME-53).
#[path = "../agent_protocol.rs"]
#[allow(dead_code)]
mod agent_protocol;

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::{
    Router,
    extract::State,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use agent_protocol::{
    AgentMessage, ClientMessage, DirEntry, FileContents, GlobResult, GrepMatch, GrepResult, PROTOCOL_VERSION,
    Reply, SkippedFile, Stream,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};

/// Loopback only. The WebSocket runs commands with no login, and the pod's
/// Docker containers share its network: on 0.0.0.0 any of them could
/// reach this at the bridge gateway (SME-33). smelt reaches it through a
/// Kubernetes port-forward, which dials localhost in the pod.
const LISTEN_ADDR: &str = "127.0.0.1:8088";
/// The relay into the pod's Docker networks (SME-33): a port-forward can
/// only dial localhost, so smelt reaches a container's IP by connecting
/// here and naming it. Loopback only, like `LISTEN_ADDR`.
const RELAY_ADDR: &str = "127.0.0.1:8089";
/// What the relay answers once it has connected to the target.
const RELAY_CONNECTED: &[u8] = b"ok\n";
/// The one line a relay connection starts with, `ip:port\n`, is at most this long.
const RELAY_TARGET_MAX: usize = 64;
const PID_FILE: &str = "/tmp/sandbox_agent.pid";
const MARKER_PREFIX: &str = "MARKER:";
/// Bounded retry for the one real race left once PID discovery moved from
/// eager (right after spawn) to reactive (only when `send_signal` is
/// called): a `send_signal` arriving essentially back-to-back with the
/// command that started it, before bash has necessarily finished forking.
/// Not measured — see SME-9's Open Questions.
const SIGNAL_DISCOVERY_RETRIES: u32 = 10;
const SIGNAL_DISCOVERY_INTERVAL: Duration = Duration::from_millis(10);

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// SHA-256 hex digest of a file's full content — what `read_file` returns
/// alongside its (possibly paginated) slice, and what `edit_file`/
/// `write_file` compare an `expected_hash` against before writing. See
/// SME-11's "Change detection, not just 'was it
/// read.'"
fn hash_content(content: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

/// Every non-overlapping byte offset where `needle` occurs in `haystack`,
/// in order — the basis for `edit_file`'s "exactly one match" / `replace_all`
/// / `expected_line` logic (see `apply_edit`). Non-overlapping so a needle
/// like "aa" against "aaa" reports one match, not two — matches ordinary
/// substring-replace semantics (`str::replace`'s own behavior), not every
/// possible overlapping window.
fn find_matches(haystack: &str, needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut offsets = Vec::new();
    let mut search_from = 0;
    while let Some(pos) = haystack[search_from..].find(needle) {
        let offset = search_from + pos;
        offsets.push(offset);
        search_from = offset + needle.len();
    }
    offsets
}

/// 1-indexed line number containing a byte offset — how a `find_matches`
/// result is checked against `edit_file`'s `expected_line`, and how
/// `read_file`'s line-numbered output is produced. Counts newlines strictly
/// before `offset`.
fn byte_offset_to_line(text: &str, offset: usize) -> u32 {
    1 + text.as_bytes()[..offset]
        .iter()
        .filter(|&&b| b == b'\n')
        .count() as u32
}

/// `edit_file`'s core replace logic — see
/// SME-11's "What"/"How" on `edit_file`.
#[derive(Debug, PartialEq)]
enum EditError {
    /// `old_string` doesn't occur in the file at all.
    NotFound,
    /// Multiple matches and neither `replace_all` nor `expected_line` was
    /// given to disambiguate which one was meant.
    Ambiguous { count: usize },
    /// `expected_line` was given, but no match starts on that line (there
    /// may still be matches elsewhere in the file).
    NoMatchAtLine { line: u32 },
}

fn apply_edit(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    expected_line: Option<u32>,
) -> Result<String, EditError> {
    let matches = find_matches(content, old_string);
    if matches.is_empty() {
        return Err(EditError::NotFound);
    }

    if replace_all {
        return Ok(content.replace(old_string, new_string));
    }

    let offset = if let Some(line) = expected_line {
        *matches
            .iter()
            .find(|&&offset| byte_offset_to_line(content, offset) == line)
            .ok_or(EditError::NoMatchAtLine { line })?
    } else if matches.len() > 1 {
        return Err(EditError::Ambiguous {
            count: matches.len(),
        });
    } else {
        matches[0]
    };

    let mut result = String::with_capacity(content.len());
    result.push_str(&content[..offset]);
    result.push_str(new_string);
    result.push_str(&content[offset + old_string.len()..]);
    Ok(result)
}

/// `read_file`'s pagination — `offset` is a 1-indexed starting line
/// (already defaulted/clamped by the caller, same convention as
/// `read_terminal_output`'s offset/limit). Returns the requested slice of
/// lines plus the file's *total* line count, so a partial read is
/// distinguishable from the whole file.
fn paginate_lines(content: &str, offset: u32, limit: u32) -> (Vec<String>, usize) {
    let all_lines: Vec<&str> = content.lines().collect();
    let total = all_lines.len();
    let skip = offset.saturating_sub(1) as usize;
    let slice = all_lines
        .into_iter()
        .skip(skip)
        .take(limit as usize)
        .map(str::to_string)
        .collect();
    (slice, total)
}

/// Compiles `pattern` into a matcher shared by `glob`'s own `pattern` and
/// `grep`'s optional `glob` filter — see
/// SME-19's "Decisions from review."
/// `literal_separator(true)` is deliberate, not `globset`'s own default:
/// checked against the real crate source (not assumed), a bare
/// `globset::Glob` lets `*` cross a `/` (so plain `*.rs`, with no `**`,
/// would already match `src/main.rs`) — indistinguishable from `**/*.rs`
/// and defeating the whole point of documenting `**` as the recursive
/// marker in this tool's own schema. `literal_separator(true)` restores
/// the shell/ripgrep-familiar meaning: a bare `*` matches within one path
/// segment, only an explicit `**/` recurses.
fn compile_glob_pattern(pattern: &str) -> Result<globset::GlobMatcher, String> {
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|e| format!("invalid glob pattern {pattern:?}: {e}"))
}

/// Compiles `pattern` for `grep` — shared error-formatting wrapper around
/// `regex::RegexBuilder`, same reason as `compile_glob_pattern`.
fn compile_grep_pattern(pattern: &str, case_insensitive: bool) -> Result<regex::Regex, String> {
    regex::RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|e| format!("invalid regex pattern {pattern:?}: {e}"))
}

/// Scans `content` line by line for `pattern`, returning up to `budget`
/// `(1-indexed line number, line text)` matches and whether `budget` was
/// exhausted before every line was checked (distinct from the file simply
/// having no more matches). One file's worth of work — the walk loop that
/// calls this per file (only exercised by the real-cluster integration
/// test, not unit-tested here — see SME-19)
/// owns combining this across every file into the overall `scan_capped`.
fn grep_content(
    content: &str,
    pattern: &regex::Regex,
    budget: usize,
) -> (Vec<(u32, String)>, bool) {
    let mut matches = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if matches.len() >= budget {
            return (matches, true);
        }
        if pattern.is_match(line) {
            matches.push((i as u32 + 1, line.to_string()));
        }
    }
    (matches, false)
}

/// Same offset/limit slicing as `paginate_lines`, generalized to an
/// already-materialized slice rather than `&str` split into lines — what
/// `glob`/`grep` page their (pre-capped, see `MAX_GLOB_SCAN`/
/// `MAX_GREP_SCAN`) match lists with. `total` is `items.len()`, not the
/// returned page's length.
fn paginate_slice<T: Clone>(items: &[T], offset: u32, limit: u32) -> (Vec<T>, usize) {
    let total = items.len();
    let skip = offset.saturating_sub(1) as usize;
    let page = items
        .iter()
        .skip(skip)
        .take(limit as usize)
        .cloned()
        .collect();
    (page, total)
}

/// Alphabetical by name, case-sensitive (Rust's default `str` ordering —
/// uppercase sorts before lowercase) — `list_directory` doesn't group
/// directories first, just a plain sort, see SME-11's "What."
fn sort_entries(mut entries: Vec<DirEntry>) -> Vec<DirEntry> {
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// What `edit_file` reports back to the model for each `EditError` variant
/// — the structured error SME-11's "Which files" bullet calls for (hash
/// mismatch / not found / ambiguous with match count / no match at the
/// given line).
fn edit_error_message(err: EditError) -> String {
    match err {
        EditError::NotFound => "old_string not found in file".to_string(),
        EditError::Ambiguous { count } => {
            format!(
                "old_string matches {count} times; add more surrounding context, set replace_all, or set expected_line to target one occurrence"
            )
        }
        EditError::NoMatchAtLine { line } => {
            format!("old_string does not match at line {line}")
        }
    }
}

/// One line of output, or the completion marker — what a terminal's reader
/// task (below) turns its shell's raw stdout/stderr into, tagged with
/// which terminal it came from since every terminal's reader now feeds one
/// shared channel. `seq` resets to 0 every time a `Marker` is emitted,
/// since it's scoped to the command that just finished, not the shell's
/// whole lifetime — matches the WS protocol's per-command `seq` exactly.
enum ShellEvent {
    Line {
        terminal_id: String,
        stream: Stream,
        seq: u64,
        data: String,
    },
    Marker {
        terminal_id: String,
        exit_code: i32,
    },
}

/// Reads one terminal's stdout and stderr concurrently for as long as its
/// shell lives, filtering the one internal line (the completion marker)
/// before anything reaches a client — the same filtering this design has
/// always done. One of these runs per terminal (spawned by
/// `create_terminal`, not just once at agent startup); while idle (no
/// command in flight) both branches simply stay pending, costing nothing.
async fn run_reader(
    terminal_id: String,
    mut stdout: BufReader<ChildStdout>,
    mut stderr: BufReader<ChildStderr>,
    tx: mpsc::UnboundedSender<ShellEvent>,
) {
    let mut seq: u64 = 0;
    loop {
        let mut out_line = String::new();
        let mut err_line = String::new();
        tokio::select! {
            result = stdout.read_line(&mut out_line) => {
                match result {
                    Ok(0) | Err(_) => break, // bash exited or pipe error
                    Ok(_) => {
                        let line = out_line.trim_end_matches('\n');
                        if let Some(rest) = line.strip_prefix(MARKER_PREFIX) {
                            let exit_code: i32 = rest.trim().parse().unwrap_or(-1);
                            if tx.send(ShellEvent::Marker { terminal_id: terminal_id.clone(), exit_code }).is_err() {
                                break;
                            }
                            seq = 0;
                        } else {
                            seq += 1;
                            if tx
                                .send(ShellEvent::Line {
                                    terminal_id: terminal_id.clone(),
                                    stream: Stream::Stdout,
                                    seq,
                                    data: line.to_string(),
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            }
            result = stderr.read_line(&mut err_line) => {
                match result {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = err_line.trim_end_matches('\n');
                        seq += 1;
                        if tx
                            .send(ShellEvent::Line {
                                terminal_id: terminal_id.clone(),
                                stream: Stream::Stderr,
                                seq,
                                data: line.to_string(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// One named terminal's state — what `AppState` used to hold once, at the
/// top level, back when an agent hosted exactly one terminal.
struct Shell {
    stdin: AsyncMutex<ChildStdin>,
    /// Also this shell's own process group id: spawned with
    /// `.process_group(0)`, so `bash_pid` and the shell's pgid are the same
    /// number — see `terminate_terminal` and SME-9's "Per-terminal
    /// process groups."
    bash_pid: u32,
    /// The one in-flight command's id in *this* terminal, if any — the
    /// single source of truth the single-command-in-flight-per-terminal
    /// design relies on. The *primary* enforcement of "only one at a time"
    /// is server-side (before a command is ever sent here at all — see
    /// SME-9's `sandbox.rs` bullet); this is the agent's own defensive
    /// backstop.
    current: AsyncMutex<Option<String>>,
    /// Held only so the child isn't dropped early; never otherwise read.
    /// Not killed on drop (tokio's default) — `terminate_terminal`'s
    /// explicit `killpg` is what actually ends it.
    _bash_child: AsyncMutex<Child>,
}

struct AppState {
    terminals: AsyncMutex<HashMap<String, Arc<Shell>>>,
    /// Kept alive here (in addition to being cloned into every terminal's
    /// reader task) so the channel never closes just because the map of
    /// terminals happens to be momentarily empty — a pod legitimately has
    /// zero terminals between `terminate_terminal` and the next
    /// `create_terminal`, and the connection must survive that.
    events_tx: mpsc::UnboundedSender<ShellEvent>,
    events_rx: AsyncMutex<mpsc::UnboundedReceiver<ShellEvent>>,
    /// The connection that gets events, and the way to tell it a newer one
    /// has taken over (`take_over`).
    current: std::sync::Mutex<Option<(u64, oneshot::Sender<()>)>>,
    next_connection: AtomicU64,
}

fn new_state() -> Arc<AppState> {
    let (tx, rx) = mpsc::unbounded_channel();
    Arc::new(AppState {
        terminals: AsyncMutex::new(HashMap::new()),
        events_tx: tx,
        events_rx: AsyncMutex::new(rx),
        current: std::sync::Mutex::new(None),
        next_connection: AtomicU64::new(0),
    })
}

fn router(state: Arc<AppState>) -> Router {
    Router::new().route("/ws", get(ws_handler)).with_state(state)
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

impl AppState {
    /// Makes the calling connection the current one, and tells the previous
    /// one, if any, to close. The returned receiver fires when a newer
    /// connection does the same to this one.
    fn take_over(&self) -> (u64, oneshot::Receiver<()>) {
        let (replace_tx, replaced) = oneshot::channel();
        let id = self.next_connection.fetch_add(1, Ordering::Relaxed);
        let previous = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace((id, replace_tx));
        if let Some((_, previous)) = previous {
            let _ = previous.send(());
        }
        (id, replaced)
    }

    /// Clears the current connection if it's still `id`.
    fn release(&self, id: u64) {
        let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if current.as_ref().is_some_and(|(current, _)| *current == id) {
            *current = None;
        }
    }
}

/// One connection at a time, and the newest wins (SME-53). A second
/// connection used to share `events_rx` with the first, so each event went
/// to whichever held it, and a half-open connection left by a smelt restart
/// could swallow a command's exit. Now the older one closes. Events that
/// arrive while no connection is open wait in `events_rx` for the next one,
/// so an exit during a smelt restart still reaches it.
async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    let (connection, mut replaced) = state.take_over();
    send_message(&mut socket, &AgentMessage::Hello(PROTOCOL_VERSION)).await;
    loop {
        tokio::select! {
            // Checked first, so a replaced connection takes no more events.
            biased;
            _ = &mut replaced => {
                tracing::info!(connection, "a newer connection replaced this one");
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        handle_client_message(&text, &state, &mut socket).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            event = recv_shell_event(&state) => {
                match event {
                    Some(ShellEvent::Line { terminal_id, stream, seq, data }) => {
                        let shell = state.terminals.lock().await.get(&terminal_id).cloned();
                        let Some(shell) = shell else { continue };
                        let id = shell.current.lock().await.clone().unwrap_or_default();
                        send_message(&mut socket, &AgentMessage::Output { id, terminal_id, stream, seq, data }).await;
                    }
                    Some(ShellEvent::Marker { terminal_id, exit_code }) => {
                        let shell = state.terminals.lock().await.get(&terminal_id).cloned();
                        let Some(shell) = shell else { continue };
                        let id = shell.current.lock().await.take();
                        if let Some(id) = id {
                            send_message(&mut socket, &AgentMessage::Exit { id, terminal_id, code: exit_code }).await;
                        }
                    }
                    None => break, // AppState (and its events_tx) dropped — agent shutting down
                }
            }
        }
    }
    state.release(connection);
}

async fn recv_shell_event(state: &Arc<AppState>) -> Option<ShellEvent> {
    state.events_rx.lock().await.recv().await
}

async fn send_message(socket: &mut WebSocket, message: &AgentMessage) {
    match serde_json::to_string(message) {
        Ok(text) => {
            let _ = socket.send(Message::Text(text.into())).await;
        }
        Err(e) => tracing::error!(%e, "couldn't serialize a message for smelt"),
    }
}

/// Handles one message from smelt. A request (anything with a
/// `request_id`) gets exactly one reply; `command` and `signal` get none.
async fn handle_client_message(text: &str, state: &Arc<AppState>, socket: &mut WebSocket) {
    let msg = match serde_json::from_str::<ClientMessage>(text) {
        Ok(msg) => msg,
        Err(e) => {
            // Where and what kind of error, never the text or serde's own
            // message: a `write_file` carries a whole file, and serde quotes
            // the values it chokes on.
            tracing::warn!(
                kind = ?e.classify(),
                line = e.line(),
                column = e.column(),
                bytes = text.len(),
                "unparseable client message"
            );
            // If the request id is still readable, smelt can fail that one
            // request at once instead of waiting it out.
            let request_id = serde_json::from_str::<serde_json::Value>(text)
                .ok()
                .and_then(|value| value.get("request_id")?.as_u64());
            send_message(socket, &AgentMessage::ProtocolError { request_id, message: e.to_string() }).await;
            return;
        }
    };

    let (request_id, result) = match msg {
        ClientMessage::Command {
            terminal_id,
            id,
            command,
        } => return start_command(state, &terminal_id, id, &command).await,
        ClientMessage::Signal {
            terminal_id,
            id,
            signal,
        } => return signal_current_command(state, &terminal_id, &id, &signal).await,
        ClientMessage::CreateTerminal { request_id, terminal_id } => {
            (request_id, create_terminal(state, terminal_id).await)
        }
        ClientMessage::TerminateTerminal { request_id, terminal_id } => {
            (request_id, terminate_terminal(state, terminal_id).await)
        }
        ClientMessage::ReadFile {
            request_id,
            path,
            offset,
            limit,
        } => (request_id, handle_read_file(path, offset, limit).await),
        ClientMessage::WriteFile {
            request_id,
            path,
            content,
            expected_hash,
        } => (request_id, handle_write_file(path, content, expected_hash).await),
        ClientMessage::EditFile {
            request_id,
            path,
            old_string,
            new_string,
            replace_all,
            expected_hash,
            expected_line,
        } => (
            request_id,
            handle_edit_file(path, old_string, new_string, replace_all, expected_hash, expected_line).await,
        ),
        ClientMessage::ListDirectory { request_id, path } => {
            (request_id, handle_list_directory(path).await)
        }
        ClientMessage::Glob {
            request_id,
            path,
            pattern,
            offset,
            limit,
        } => (request_id, handle_glob(path, pattern, offset, limit).await),
        ClientMessage::Grep {
            request_id,
            path,
            pattern,
            glob,
            case_insensitive,
            offset,
            limit,
        } => (
            request_id,
            handle_grep(path, pattern, glob, case_insensitive, offset, limit).await,
        ),
    };
    send_message(socket, &AgentMessage::Reply { request_id, result }).await;
}

/// Shared across `read_file`/`edit_file`/`write_file` — an unbounded file
/// becomes an unbounded tool-result message persisted into Postgres and
/// re-sent on every subsequent turn, see
/// SME-11's "Size bound."
const MAX_FILE_SIZE_BYTES: u64 = 256 * 1024;
/// `list_directory`'s analogous bound, as an entry count rather than bytes
/// — same reasoning, see SME-11's "Size bound."
const MAX_DIR_ENTRIES: usize = 1000;
/// `grep` skips (never errors out entirely on, see `SkippedFile`) a
/// file bigger than this — bounds worst-case per-file scan time. Distinct
/// from `MAX_FILE_SIZE_BYTES` above, which caps *returned* content
/// (`read_file`'s concern); grep's returned payload is already bounded by
/// its match count, not the size of what it searched through.
const MAX_GREP_FILE_SIZE_BYTES: u64 = 5 * 1024 * 1024;
/// Hard ceiling on matches `glob`/`grep` accumulate across the whole walk,
/// independent of the page (`offset`/`limit`) actually requested — bounds
/// worst-case walk cost on a huge tree or a pathological pattern.
/// `scan_capped` in the response is true only when *this* was hit, not
/// just the requested page — see
/// SME-19's "Decisions from review."
const MAX_GLOB_SCAN: usize = 2000;
const MAX_GREP_SCAN: usize = 1000;

fn reply_error(message: &str) -> Reply {
    Reply::Error { message: message.to_string() }
}

async fn handle_read_file(
    path: String,
    offset: u32,
    limit: u32,
) -> Reply {
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(e) => {
            return reply_error(&format!("failed to read {path}: {e}"));
        }
    };
    if bytes.len() as u64 > MAX_FILE_SIZE_BYTES {
        return reply_error(&format!("{path} exceeds the {MAX_FILE_SIZE_BYTES}-byte size limit"));
    }
    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(_) => {
            return reply_error(&format!("{path} is not valid UTF-8"));
        }
    };
    let hash = hash_content(content.as_bytes());
    let (lines, total_lines) = paginate_lines(&content, offset, limit);
    Reply::FileRead(FileContents { lines, total_lines, hash })
}

async fn handle_write_file(
    path: String,
    content: String,
    expected_hash: Option<String>,
) -> Reply {
    if content.len() as u64 > MAX_FILE_SIZE_BYTES {
        return reply_error(&format!("content exceeds the {MAX_FILE_SIZE_BYTES}-byte size limit"));
    }

    if let Some(expected) = &expected_hash {
        match tokio::fs::read(&path).await {
            Ok(current) => {
                let current_hash = hash_content(&current);
                if &current_hash != expected {
                    return reply_error(&format!("{path} has changed since it was last read (expected hash {expected}, found {current_hash}); read_file again before writing"));
                }
            }
            Err(e) => {
                return reply_error(&format!("failed to read {path} for hash check: {e}"));
            }
        }
    }

    if let Some(parent) = std::path::Path::new(&path).parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return reply_error(&format!("failed to create parent directories for {path}: {e}"));
            }
        }
    }

    if let Err(e) = tokio::fs::write(&path, content.as_bytes()).await {
        return reply_error(&format!("failed to write {path}: {e}"));
    }
    let hash = hash_content(content.as_bytes());
    Reply::FileWritten { hash }
}

async fn handle_edit_file(
    path: String,
    old_string: String,
    new_string: String,
    replace_all: bool,
    expected_hash: String,
    expected_line: Option<u32>,
) -> Reply {
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(e) => {
            return reply_error(&format!("failed to read {path}: {e}"));
        }
    };
    let current_hash = hash_content(&bytes);
    if current_hash != expected_hash {
        return reply_error(&format!("{path} has changed since it was last read (expected hash {expected_hash}, found {current_hash}); read_file again before editing"));
    }
    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(_) => {
            return reply_error(&format!("{path} is not valid UTF-8"));
        }
    };

    let new_content = match apply_edit(
        &content,
        &old_string,
        &new_string,
        replace_all,
        expected_line,
    ) {
        Ok(new_content) => new_content,
        Err(edit_err) => {
            return reply_error(&edit_error_message(edit_err));
        }
    };

    if let Err(e) = tokio::fs::write(&path, new_content.as_bytes()).await {
        return reply_error(&format!("failed to write {path}: {e}"));
    }
    let hash = hash_content(new_content.as_bytes());
    Reply::FileEdited { hash }
}

async fn handle_list_directory(path: String) -> Reply {
    let mut read_dir = match tokio::fs::read_dir(&path).await {
        Ok(read_dir) => read_dir,
        Err(e) => {
            return reply_error(&format!("failed to read directory {path}: {e}"));
        }
    };

    let mut entries = Vec::new();
    loop {
        match read_dir.next_entry().await {
            Ok(Some(entry)) => {
                if entries.len() >= MAX_DIR_ENTRIES {
                    return reply_error(&format!("{path} has more than {MAX_DIR_ENTRIES} entries"));
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                let size = if is_dir {
                    None
                } else {
                    entry.metadata().await.ok().map(|m| m.len())
                };
                entries.push(DirEntry { name, is_dir, size });
            }
            Ok(None) => break,
            Err(e) => {
                return reply_error(&format!("failed to read directory {path}: {e}"));
            }
        }
    }

    let entries = sort_entries(entries);
    Reply::DirectoryListed { entries }
}

/// Finds files under `path` whose path relative to `path` matches
/// `pattern` — see SME-19. Real filesystem
/// walk (via `ignore::WalkBuilder`, `.gitignore`-aware even outside a real
/// git checkout), not unit-tested directly — only `compile_glob_pattern`
/// and `paginate_slice`, its pure pieces, are; this glue is exercised by
/// the real-cluster integration test instead, same as
/// `handle_list_directory` above already is.
async fn handle_glob(
    path: String,
    pattern: String,
    offset: u32,
    limit: u32,
) -> Reply {
    let matcher = match compile_glob_pattern(&pattern) {
        Ok(matcher) => matcher,
        Err(e) => {
            return reply_error(&e);
        }
    };
    let walked = tokio::task::spawn_blocking(move || walk_glob(&path, &matcher)).await;
    let (all_paths, scan_capped) = match walked {
        Ok(result) => result,
        Err(_) => {
            return reply_error("internal error walking directory");
        }
    };
    let (paths, total) = paginate_slice(&all_paths, offset, limit);
    Reply::GlobMatched(GlobResult { paths, total, scan_capped })
}

/// Synchronous (`ignore`'s walker is not async) — must run inside
/// `spawn_blocking`, never called directly from async code. Stops once
/// `MAX_GLOB_SCAN` matches are found, reporting that as `scan_capped`
/// rather than silently truncating.
fn walk_glob(root: &str, matcher: &globset::GlobMatcher) -> (Vec<String>, bool) {
    let root_path = std::path::Path::new(root);
    let mut paths = Vec::new();
    let mut scan_capped = false;
    let walker = ignore::WalkBuilder::new(root_path)
        .git_ignore(true)
        .require_git(false)
        .build();
    for entry in walker {
        let Ok(entry) = entry else { continue };
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            continue;
        }
        let relative = entry.path().strip_prefix(root_path).unwrap_or(entry.path());
        if !matcher.is_match(relative) {
            continue;
        }
        if paths.len() >= MAX_GLOB_SCAN {
            scan_capped = true;
            break;
        }
        paths.push(entry.path().to_string_lossy().into_owned());
    }
    (paths, scan_capped)
}

/// Searches file contents under `path` for `pattern`, optionally narrowed
/// to files matching `glob` first — same walk/testing split as
/// `handle_glob` above.
async fn handle_grep(
    path: String,
    pattern: String,
    glob: Option<String>,
    case_insensitive: bool,
    offset: u32,
    limit: u32,
) -> Reply {
    let pattern = match compile_grep_pattern(&pattern, case_insensitive) {
        Ok(pattern) => pattern,
        Err(e) => {
            return reply_error(&e);
        }
    };
    let glob_matcher = match glob.as_deref().map(compile_glob_pattern) {
        Some(Ok(matcher)) => Some(matcher),
        Some(Err(e)) => {
            return reply_error(&e);
        }
        None => None,
    };
    let walked =
        tokio::task::spawn_blocking(move || walk_grep(&path, &pattern, glob_matcher.as_ref()))
            .await;
    let (all_matches, scan_capped, skipped) = match walked {
        Ok(result) => result,
        Err(_) => {
            return reply_error("internal error walking directory");
        }
    };
    let (matches, total) = paginate_slice(&all_matches, offset, limit);
    Reply::GrepMatched(GrepResult { matches, total, scan_capped, skipped })
}

/// Synchronous, same `spawn_blocking`-only rule as `walk_glob`. Every file
/// it excludes (oversized, or not valid UTF-8) lands in the returned
/// `skipped` list — never silently dropped, per SME-19's "Decisions
/// from review." `scan_capped` combines the walk's own cap with
/// `grep_content`'s per-file budget exhaustion — either one means there
/// may be more matches than `total` reports.
fn walk_grep(
    root: &str,
    pattern: &regex::Regex,
    glob_matcher: Option<&globset::GlobMatcher>,
) -> (Vec<GrepMatch>, bool, Vec<SkippedFile>) {
    let root_path = std::path::Path::new(root);
    let mut matches = Vec::new();
    let mut skipped = Vec::new();
    let mut scan_capped = false;
    let walker = ignore::WalkBuilder::new(root_path)
        .git_ignore(true)
        .require_git(false)
        .build();
    for entry in walker {
        if matches.len() >= MAX_GREP_SCAN {
            scan_capped = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            continue;
        }
        let relative = entry.path().strip_prefix(root_path).unwrap_or(entry.path());
        if let Some(matcher) = glob_matcher {
            if !matcher.is_match(relative) {
                continue;
            }
        }
        let display_path = entry.path().to_string_lossy().into_owned();
        let size = match entry.metadata() {
            Ok(metadata) => metadata.len(),
            Err(_) => continue,
        };
        if size > MAX_GREP_FILE_SIZE_BYTES {
            skipped.push(SkippedFile {
                path: display_path,
                reason: format!("too large ({size} bytes > {MAX_GREP_FILE_SIZE_BYTES} byte limit)"),
            });
            continue;
        }
        let bytes = match std::fs::read(entry.path()) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let content = match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(_) => {
                skipped.push(SkippedFile {
                    path: display_path,
                    reason: "binary".to_string(),
                });
                continue;
            }
        };
        let budget = MAX_GREP_SCAN.saturating_sub(matches.len());
        let (file_matches, file_capped) = grep_content(&content, pattern, budget);
        if file_capped {
            scan_capped = true;
        }
        for (line, text) in file_matches {
            matches.push(GrepMatch {
                path: display_path.clone(),
                line,
                text,
            });
        }
    }
    (matches, scan_capped, skipped)
}

/// Spawns a new named shell — `.process_group(0)` gives it a process group
/// distinct from the agent's and from every sibling terminal's (see
/// SME-9's "Per-terminal process groups"), so `terminate_terminal` can
/// `killpg` it in isolation later. `set -m`/`trap ':' INT` are the same
/// per-shell startup this design has always used, just no longer only at
/// agent-launch time.
async fn create_terminal(state: &Arc<AppState>, terminal_id: String) -> Reply {
    if state.terminals.lock().await.contains_key(&terminal_id) {
        return reply_error("terminal_id already exists");
    }

    let mut child = match Command::new("bash")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return reply_error(&format!("failed to spawn shell: {e}"));
        }
    };
    let bash_pid = child
        .id()
        .expect("bash should have a pid immediately after spawn");
    let mut stdin = child
        .stdin
        .take()
        .expect("stdin requested via Stdio::piped()");
    let stdout = BufReader::new(
        child
            .stdout
            .take()
            .expect("stdout requested via Stdio::piped()"),
    );
    let stderr = BufReader::new(
        child
            .stderr
            .take()
            .expect("stderr requested via Stdio::piped()"),
    );

    // set -m: job control, so every command (including a plain foreground
    // one) gets its own process group send_signal can target.
    // trap ':' INT: without this, a non-interactive bash re-raises SIGINT
    // against itself once a foreground job dies from it, taking the whole
    // shell down with it — see SME-9's "Signaling a running command."
    if let Err(e) = stdin.write_all(b"set -m\ntrap ':' INT\n").await {
        return reply_error(&format!("failed to initialize shell: {e}"));
    }
    let _ = stdin.flush().await;

    tokio::spawn(run_reader(
        terminal_id.clone(),
        stdout,
        stderr,
        state.events_tx.clone(),
    ));

    let shell = Arc::new(Shell {
        stdin: AsyncMutex::new(stdin),
        bash_pid,
        current: AsyncMutex::new(None),
        _bash_child: AsyncMutex::new(child),
    });
    state
        .terminals
        .lock()
        .await
        .insert(terminal_id.clone(), shell);

    Reply::TerminalCreated
}

/// `killpg` targeting just this terminal's own process group — safe in
/// isolation because `.process_group(0)` gave it one distinct from the
/// agent's and every sibling terminal's. See SME-9's "Terminating a
/// terminal without touching the pod, or its siblings."
async fn terminate_terminal(state: &Arc<AppState>, terminal_id: String) -> Reply {
    let shell = state.terminals.lock().await.remove(&terminal_id);
    let Some(shell) = shell else {
        return reply_error("unknown terminal_id");
    };

    if let Err(err) = signal::kill(Pid::from_raw(-(shell.bash_pid as i32)), Signal::SIGKILL) {
        tracing::warn!(%err, %terminal_id, "killpg failed while terminating terminal");
    }

    Reply::TerminalTerminated
}

async fn start_command(state: &Arc<AppState>, terminal_id: &str, id: String, command: &str) {
    let Some(shell) = state.terminals.lock().await.get(terminal_id).cloned() else {
        tracing::warn!(%terminal_id, %id, "rejecting command: unknown terminal_id");
        return;
    };

    let mut current = shell.current.lock().await;
    if current.is_some() {
        // Should already be prevented server-side (see SME-9's plan) — this is
        // a defensive backstop, not the primary enforcement, so a quiet
        // log rather than inventing a new protocol error message.
        tracing::warn!(%id, "rejecting command: another is already in flight in this terminal");
        return;
    }
    *current = Some(id);
    drop(current);

    let payload = format!(
        "eval {}\necho \"{MARKER_PREFIX}$?\"\n",
        shell_quote(command)
    );
    let mut stdin = shell.stdin.lock().await;
    if stdin.write_all(payload.as_bytes()).await.is_ok() {
        let _ = stdin.flush().await;
    }
}

async fn signal_current_command(state: &Arc<AppState>, terminal_id: &str, id: &str, signal: &str) {
    let Some(shell) = state.terminals.lock().await.get(terminal_id).cloned() else {
        tracing::info!(%terminal_id, "send_signal: unknown terminal_id, ignoring");
        return;
    };

    let matches_current = shell.current.lock().await.as_deref() == Some(id);
    if !matches_current {
        tracing::info!(%id, "send_signal: id does not match the in-flight command, ignoring");
        return;
    }

    let Some(sig) = parse_signal(signal) else {
        tracing::warn!(%signal, "send_signal: unrecognized signal name, ignoring");
        return;
    };

    let Some(pgid) = discover_current_job_pgid(shell.bash_pid).await else {
        tracing::info!(
            "send_signal: no running child found (already finished, or a bare builtin) — \
             nothing to signal"
        );
        return;
    };
    // Negative pid is the standard POSIX convention for "signal the whole
    // process group" — used instead of a separate killpg call so this
    // doesn't depend on that being a distinct nix function.
    if let Err(err) = signal::kill(Pid::from_raw(-pgid), sig) {
        tracing::warn!(%err, pgid, "killpg failed");
    }
}

fn parse_signal(name: &str) -> Option<Signal> {
    match name {
        "INT" => Some(Signal::SIGINT),
        "TERM" => Some(Signal::SIGTERM),
        "KILL" => Some(Signal::SIGKILL),
        _ => None,
    }
}

/// Reactive, not eager — read only when `send_signal` is actually called,
/// with a short bounded retry for the one real race (a `send_signal`
/// arriving essentially back-to-back with the command that started it).
/// See SME-9's "Discovering a running command's process group."
async fn discover_current_job_pgid(bash_pid: u32) -> Option<i32> {
    let children_path = format!("/proc/{bash_pid}/task/{bash_pid}/children");
    for _ in 0..SIGNAL_DISCOVERY_RETRIES {
        if let Ok(contents) = tokio::fs::read_to_string(&children_path).await {
            if let Some(first_child) = contents.split_whitespace().next() {
                if let Ok(pid) = first_child.parse::<i32>() {
                    return Some(pgid_of(pid).unwrap_or(pid));
                }
            }
        }
        tokio::time::sleep(SIGNAL_DISCOVERY_INTERVAL).await;
    }
    None
}

/// Field 4 (1-indexed) after the closing `)` of the `comm` field in
/// `/proc/<pid>/stat` is `pgrp` — see `proc(5)`. `comm` can itself contain
/// spaces/parens, so splitting after the *last* `)` is what makes this
/// robust rather than naively splitting on whitespace from the start.
fn pgid_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    after_comm.split_whitespace().nth(2)?.parse().ok()
}

/// Where a relay connection asks to go: `ip:port` for a container in the
/// pod's Docker range, and nothing else, so the relay can't reach the
/// cluster or the LAN.
fn relay_target(line: &str) -> Result<std::net::SocketAddrV4, String> {
    let (host, port) = line
        .rsplit_once(':')
        .ok_or_else(|| format!("{line:?} isn't ip:port"))?;
    let ip = docker_net::container_address(host)
        .ok_or_else(|| format!("{host:?} isn't a container address in the pod"))?;
    let port: u16 = port
        .parse()
        .ok()
        .filter(|&p| p != 0)
        .ok_or_else(|| format!("{port:?} isn't a port"))?;
    Ok(std::net::SocketAddrV4::new(ip, port))
}

/// Serves `RELAY_ADDR`: each connection names a container address on its
/// first line. Once connected, the relay answers `RELAY_CONNECTED`, then
/// carries raw bytes to and from it. A refused or failed target just
/// closes the connection, the same as a port nothing listens on, which is
/// how smelt reports it. The answer matters: a connect to an address no
/// container has yet hangs rather than fails, and without it smelt would
/// take that silence for a server waiting for a request.
async fn serve_relay(listener: tokio::net::TcpListener) {
    accept_loop(
        || async { listener.accept().await.map(|(client, _)| client) },
        |client| {
            tokio::spawn(async move {
                if let Err(e) = relay(client).await {
                    tracing::debug!("relay: {e}");
                }
            });
        },
    )
    .await
}

/// Accepts connections for ever, handing each to `handle`.
async fn accept_loop<S, A, Fut, H>(mut accept: A, mut handle: H)
where
    A: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<S>>,
    H: FnMut(S),
{
    loop {
        match accept().await {
            Ok(client) => handle(client),
            Err(e) => {
                // A failure like EMFILE fails again at once: wait instead of
                // spinning, as the server's own accept loops do.
                tracing::warn!("relay: accept failed: {e}");
                tokio::time::sleep(ACCEPT_RETRY).await;
            }
        }
    }
}

/// How long `accept_loop` waits after a failed accept.
const ACCEPT_RETRY: Duration = Duration::from_millis(50);

async fn relay(client: tokio::net::TcpStream) -> Result<(), String> {
    let mut client = BufReader::new(client);
    let mut line = String::new();
    let read = tokio::time::timeout(
        Duration::from_secs(5),
        (&mut client).take(RELAY_TARGET_MAX as u64).read_line(&mut line),
    )
    .await
    .map_err(|_| "timed out waiting for the target".to_string())?
    .map_err(|e| e.to_string())?;
    if read == 0 || !line.ends_with('\n') {
        return Err("no target line".to_string());
    }
    let target = relay_target(line.trim_end())?;
    let mut upstream = tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(target))
        .await
        .map_err(|_| format!("timed out connecting to {target}"))?
        .map_err(|e| format!("couldn't connect to {target}: {e}"))?;
    // Bytes the client sent after its target line are already buffered.
    let buffered = client.buffer().to_vec();
    let mut client = client.into_inner();
    client.write_all(RELAY_CONNECTED).await.map_err(|e| e.to_string())?;
    upstream.write_all(&buffered).await.map_err(|e| e.to_string())?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_relay_target_accepts_a_container_address_and_port() {
        assert_eq!(
            relay_target("172.21.0.2:3000"),
            Ok("172.21.0.2:3000".parse().unwrap())
        );
    }

    #[test]
    fn test_relay_target_refuses_anything_outside_the_docker_range() {
        for line in [
            "10.43.0.1:443",
            "192.168.8.130:6443",
            "127.0.0.1:8088",
            "localhost:3000",
            "172.21.0.2",
            "172.21.0.2:0",
            "172.21.0.2:99999",
            "",
        ] {
            assert!(relay_target(line).is_err(), "{line:?} should be refused");
        }
    }

    #[test]
    fn test_hash_content_returns_sha256_hex_digest() {
        // Known SHA-256("hello") test vector.
        assert_eq!(
            hash_content(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_find_matches_returns_every_non_overlapping_occurrence() {
        assert_eq!(find_matches("abcabcabc", "abc"), vec![0, 3, 6]);
    }

    #[test]
    fn test_find_matches_none_found() {
        assert_eq!(find_matches("hello world", "xyz"), Vec::<usize>::new());
    }

    #[test]
    fn test_find_matches_does_not_double_count_overlapping_windows() {
        // "aa" against "aaa": ordinary (non-overlapping) substring-replace
        // semantics report one match at offset 0, not two.
        assert_eq!(find_matches("aaa", "aa"), vec![0]);
    }

    #[test]
    fn test_byte_offset_to_line_first_line_is_one() {
        assert_eq!(byte_offset_to_line("hello\nworld", 0), 1);
    }

    #[test]
    fn test_byte_offset_to_line_counts_preceding_newlines() {
        let text = "line1\nline2\nline3";
        let offset_of_line3 = text.find("line3").unwrap();
        assert_eq!(byte_offset_to_line(text, offset_of_line3), 3);
    }

    #[test]
    fn test_byte_offset_to_line_mid_line_offset_still_counts_as_that_line() {
        let text = "line1\nline2\nline3";
        // Points into the middle of "line2", not its start.
        let mid_line2 = text.find("line2").unwrap() + 2;
        assert_eq!(byte_offset_to_line(text, mid_line2), 2);
    }

    #[test]
    fn test_apply_edit_replaces_a_unique_match() {
        let result =
            apply_edit("hello world", "world", "there", false, None).expect("should succeed");
        assert_eq!(result, "hello there");
    }

    #[test]
    fn test_apply_edit_errors_when_old_string_not_found() {
        let result = apply_edit("hello world", "xyz", "there", false, None);
        assert_eq!(result, Err(EditError::NotFound));
    }

    #[test]
    fn test_apply_edit_errors_when_ambiguous() {
        let result = apply_edit("foo foo foo", "foo", "bar", false, None);
        assert_eq!(result, Err(EditError::Ambiguous { count: 3 }));
    }

    #[test]
    fn test_apply_edit_replace_all_replaces_every_occurrence() {
        let result = apply_edit("foo foo foo", "foo", "bar", true, None).expect("should succeed");
        assert_eq!(result, "bar bar bar");
    }

    #[test]
    fn test_apply_edit_expected_line_targets_one_occurrence_among_duplicates() {
        // Two identical lines — expected_line picks the second one
        // specifically, leaving the first untouched.
        let content = "let x = 1;\nlet x = 1;\n";
        let line2_start = content.match_indices("let x = 1;").nth(1).unwrap().0;
        let expected_line = byte_offset_to_line(content, line2_start);
        let result = apply_edit(
            content,
            "let x = 1;",
            "let x = 2;",
            false,
            Some(expected_line),
        )
        .expect("should succeed");
        assert_eq!(result, "let x = 1;\nlet x = 2;\n");
    }

    #[test]
    fn test_apply_edit_expected_line_errors_when_no_match_starts_there() {
        let content = "let x = 1;\nlet y = 2;\n";
        let result = apply_edit(content, "let x = 1;", "let x = 2;", false, Some(99));
        assert_eq!(result, Err(EditError::NoMatchAtLine { line: 99 }));
    }

    #[test]
    fn test_apply_edit_multiline_old_and_new_string() {
        let content = "fn f() {\n    old_body();\n}\n";
        let result = apply_edit(
            content,
            "fn f() {\n    old_body();\n}",
            "fn f() {\n    new_body();\n    more();\n}",
            false,
            None,
        )
        .expect("should succeed");
        assert_eq!(result, "fn f() {\n    new_body();\n    more();\n}\n");
    }

    #[test]
    fn test_paginate_lines_returns_full_content_within_limit() {
        let (lines, total) = paginate_lines("a\nb\nc", 1, 10);
        assert_eq!(lines, vec!["a", "b", "c"]);
        assert_eq!(total, 3);
    }

    #[test]
    fn test_paginate_lines_respects_offset() {
        let (lines, total) = paginate_lines("a\nb\nc", 2, 10);
        assert_eq!(lines, vec!["b", "c"]);
        assert_eq!(total, 3);
    }

    #[test]
    fn test_paginate_lines_respects_limit() {
        let (lines, total) = paginate_lines("a\nb\nc", 1, 2);
        assert_eq!(lines, vec!["a", "b"]);
        assert_eq!(
            total, 3,
            "total should reflect the whole file, not just the returned slice"
        );
    }

    #[test]
    fn test_paginate_lines_offset_beyond_end_returns_empty_but_correct_total() {
        let (lines, total) = paginate_lines("a\nb\nc", 99, 10);
        assert!(lines.is_empty());
        assert_eq!(total, 3);
    }

    // --- paginate_slice: same offset/limit semantics as paginate_lines,
    // but over an already-materialized Vec<T> (glob's Vec<String> paths,
    // grep's Vec<GrepMatch>) rather than splitting `&str` into lines —
    // see SME-19.

    #[test]
    fn test_paginate_slice_returns_full_slice_within_limit() {
        let (page, total) = paginate_slice(&["a", "b", "c"], 1, 10);
        assert_eq!(page, vec!["a", "b", "c"]);
        assert_eq!(total, 3);
    }

    #[test]
    fn test_paginate_slice_respects_offset() {
        let (page, total) = paginate_slice(&["a", "b", "c"], 2, 10);
        assert_eq!(page, vec!["b", "c"]);
        assert_eq!(total, 3);
    }

    #[test]
    fn test_paginate_slice_respects_limit() {
        let (page, total) = paginate_slice(&["a", "b", "c"], 1, 2);
        assert_eq!(page, vec!["a", "b"]);
        assert_eq!(
            total, 3,
            "total should reflect the whole slice, not just the returned page"
        );
    }

    #[test]
    fn test_paginate_slice_offset_beyond_end_returns_empty_but_correct_total() {
        let (page, total): (Vec<&str>, usize) = paginate_slice(&["a", "b", "c"], 99, 10);
        assert!(page.is_empty());
        assert_eq!(total, 3);
    }

    // --- compile_glob_pattern: shared by glob's own `pattern` and grep's
    // optional `glob` filter — see SME-19's
    // "Decisions from review."

    #[test]
    fn test_compile_glob_pattern_matches_double_star_extension() {
        let matcher = compile_glob_pattern("**/*.rs").expect("valid pattern");
        assert!(matcher.is_match("src/main.rs"));
        assert!(matcher.is_match("main.rs"));
        assert!(!matcher.is_match("src/main.py"));
    }

    #[test]
    fn test_compile_glob_pattern_plain_star_does_not_cross_directories() {
        let matcher = compile_glob_pattern("*.rs").expect("valid pattern");
        assert!(matcher.is_match("main.rs"));
        assert!(
            !matcher.is_match("src/main.rs"),
            "a bare * shouldn't match across a path separator"
        );
    }

    #[test]
    fn test_compile_glob_pattern_rejects_invalid_pattern() {
        let err = compile_glob_pattern("[").expect_err("unterminated character class");
        assert!(
            err.contains("invalid glob pattern"),
            "expected a clear invalid-pattern error, got: {err}"
        );
    }

    // --- compile_grep_pattern / grep_content: grep's own regex-matching
    // core, deliberately separated from the real directory walk (which is
    // only exercised by the real-cluster integration test) — see
    // SME-19.

    #[test]
    fn test_compile_grep_pattern_rejects_invalid_regex() {
        let err = compile_grep_pattern("(", false).expect_err("unbalanced group");
        assert!(
            err.contains("invalid regex pattern"),
            "expected a clear invalid-pattern error, got: {err}"
        );
    }

    #[test]
    fn test_compile_grep_pattern_case_insensitive_matches_regardless_of_case() {
        let pattern = compile_grep_pattern("hello", true).expect("valid pattern");
        assert!(pattern.is_match("HELLO world"));
    }

    #[test]
    fn test_compile_grep_pattern_case_sensitive_by_default() {
        let pattern = compile_grep_pattern("hello", false).expect("valid pattern");
        assert!(!pattern.is_match("HELLO world"));
    }

    #[test]
    fn test_grep_content_returns_every_matching_line_with_1_indexed_line_numbers() {
        let pattern = compile_grep_pattern("fn ", false).expect("valid pattern");
        let (matches, budget_exhausted) =
            grep_content("fn a() {}\nlet x = 1;\nfn b() {}\n", &pattern, 10);
        assert_eq!(
            matches,
            vec![(1, "fn a() {}".to_string()), (3, "fn b() {}".to_string())]
        );
        assert!(!budget_exhausted);
    }

    #[test]
    fn test_grep_content_no_matches_returns_empty() {
        let pattern = compile_grep_pattern("nope", false).expect("valid pattern");
        let (matches, budget_exhausted) = grep_content("a\nb\nc\n", &pattern, 10);
        assert!(matches.is_empty());
        assert!(!budget_exhausted);
    }

    #[test]
    fn test_grep_content_stops_at_budget_and_reports_exhaustion() {
        let pattern = compile_grep_pattern("x", false).expect("valid pattern");
        let (matches, budget_exhausted) = grep_content("x\nx\nx\nx\n", &pattern, 2);
        assert_eq!(matches, vec![(1, "x".to_string()), (2, "x".to_string())]);
        assert!(
            budget_exhausted,
            "should report that more matches existed than the budget allowed"
        );
    }

    fn entry(name: &str, is_dir: bool, size: Option<u64>) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            is_dir,
            size,
        }
    }

    #[test]
    fn test_sort_entries_orders_alphabetically_case_sensitive() {
        let entries = vec![
            entry("banana", false, Some(3)),
            entry("Apple", true, None),
            entry("cherry", false, Some(5)),
        ];
        let sorted = sort_entries(entries);
        assert_eq!(
            sorted.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            // Byte-wise ordering: uppercase 'A' (0x41) sorts before
            // lowercase 'b'/'c' (0x62/0x63).
            vec!["Apple", "banana", "cherry"]
        );
    }

    #[test]
    fn test_edit_error_message_not_found() {
        assert_eq!(
            edit_error_message(EditError::NotFound),
            "old_string not found in file"
        );
    }

    #[test]
    fn test_edit_error_message_ambiguous_includes_match_count() {
        let message = edit_error_message(EditError::Ambiguous { count: 4 });
        assert!(
            message.contains('4'),
            "expected the match count in the message, got: {message}"
        );
    }

    #[test]
    fn test_edit_error_message_no_match_at_line_includes_the_line_number() {
        let message = edit_error_message(EditError::NoMatchAtLine { line: 42 });
        assert!(
            message.contains("42"),
            "expected the line number in the message, got: {message}"
        );
    }
}

/// SIGTERM's handler: `_exit` is safe to call from a signal handler.
extern "C" fn exit_on_sigterm(_: nix::libc::c_int) {
    unsafe { nix::libc::_exit(0) }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    std::fs::write(PID_FILE, std::process::id().to_string())
        .unwrap_or_else(|e| panic!("failed to write {PID_FILE}: {e}"));

    // A signal that is ignored when a process starts stays ignored across
    // exec(), and POSIX forbids a *non-interactive* bash from trapping one
    // that was ignored on entry. So if this process inherited SIGINT or
    // SIGQUIT ignored (as it did when a `sleep infinity` PID 1 launched it,
    // before it became the image's ENTRYPOINT), `trap ':' INT` in
    // `create_terminal` would silently do nothing and `send_signal`'s SIGINT
    // would never reach a command. Reset both to the default once, before
    // spawning any shell, so every terminal's `bash` starts with them
    // deliverable whatever launched the agent.
    unsafe {
        signal::signal(Signal::SIGINT, signal::SigHandler::SigDfl)
            .expect("reset SIGINT to default disposition");
        signal::signal(Signal::SIGQUIT, signal::SigHandler::SigDfl)
            .expect("reset SIGQUIT to default disposition");
        // As PID 1 with no handler, SIGTERM would be ignored, and every
        // deleted pod would wait out its whole grace period before being
        // killed. The pod is going away: exit at once (SME-51 B6).
        signal::signal(Signal::SIGTERM, signal::SigHandler::Handler(exit_on_sigterm))
            .expect("handle SIGTERM");
    }

    let app = router(new_state());
    let relay_listener = tokio::net::TcpListener::bind(RELAY_ADDR)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {RELAY_ADDR}: {e}"));
    tokio::spawn(serve_relay(relay_listener));

    let listener = tokio::net::TcpListener::bind(LISTEN_ADDR)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {LISTEN_ADDR}: {e}"));
    tracing::info!("sandbox_agent listening on {LISTEN_ADDR}");
    axum::serve(listener, app).await.expect("server error");
}

/// The agent's WebSocket end to end, in-process: its real router on a
/// loopback port, real `bash` terminals, and a tungstenite client in the
/// test (SME-53).
#[cfg(test)]
mod socket_tests {
    use super::agent_protocol::{AgentMessage, ClientMessage, PROTOCOL_VERSION, Reply, Stream};
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    type Client = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

    const WAIT: Duration = Duration::from_secs(5);

    async fn serve_agent() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, router(new_state())).await });
        addr
    }

    async fn connect(addr: std::net::SocketAddr) -> Client {
        let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let (client, _) = tokio_tungstenite::client_async(format!("ws://{addr}/ws"), stream)
            .await
            .expect("upgrade");
        client
    }

    /// Connects and reads past the hello.
    async fn connect_ready(addr: std::net::SocketAddr) -> Client {
        let mut client = connect(addr).await;
        assert_eq!(next(&mut client).await, AgentMessage::Hello(PROTOCOL_VERSION));
        client
    }

    async fn send(client: &mut Client, message: &ClientMessage) {
        let text = serde_json::to_string(message).expect("serializes");
        client.send(WsMessage::Text(text)).await.expect("send");
    }

    async fn send_raw(client: &mut Client, text: &str) {
        client.send(WsMessage::Text(text.to_string())).await.expect("send");
    }

    async fn next(client: &mut Client) -> AgentMessage {
        loop {
            let frame = tokio::time::timeout(WAIT, client.next())
                .await
                .expect("the agent sent nothing within 5s")
                .expect("the connection ended")
                .expect("a WebSocket error");
            if let WsMessage::Text(text) = frame {
                return serde_json::from_str(&text)
                    .unwrap_or_else(|e| panic!("the agent sent something that isn't v1 ({e}): {text}"));
            }
        }
    }

    /// Whether the agent closed this connection within 5s.
    async fn closed(client: &mut Client) -> bool {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            match tokio::time::timeout_at(deadline, client.next()).await {
                Err(_) => return false,
                Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(WsMessage::Close(_)))) => return true,
                Ok(Some(Ok(_))) => continue,
            }
        }
    }

    async fn create_terminal(client: &mut Client, terminal_id: &str) {
        send(
            client,
            &ClientMessage::CreateTerminal { request_id: 1, terminal_id: terminal_id.into() },
        )
        .await;
        assert_eq!(
            next(client).await,
            AgentMessage::Reply { request_id: 1, result: Reply::TerminalCreated }
        );
    }

    async fn run(client: &mut Client, terminal_id: &str, id: &str, command: &str) {
        send(
            client,
            &ClientMessage::Command { terminal_id: terminal_id.into(), id: id.into(), command: command.into() },
        )
        .await;
    }

    #[tokio::test]
    async fn test_hello_is_the_first_message_on_a_connection() {
        let addr = serve_agent().await;
        let mut client = connect(addr).await;
        assert_eq!(next(&mut client).await, AgentMessage::Hello(PROTOCOL_VERSION));
    }

    #[tokio::test]
    async fn test_terminal_actions_are_answered_with_their_request_id() {
        let addr = serve_agent().await;
        let mut client = connect_ready(addr).await;
        send(&mut client, &ClientMessage::CreateTerminal { request_id: 11, terminal_id: "t1".into() }).await;
        assert_eq!(next(&mut client).await, AgentMessage::Reply { request_id: 11, result: Reply::TerminalCreated });
        send(&mut client, &ClientMessage::CreateTerminal { request_id: 12, terminal_id: "t1".into() }).await;
        assert_eq!(
            next(&mut client).await,
            AgentMessage::Reply {
                request_id: 12,
                result: Reply::Error { message: "terminal_id already exists".into() }
            }
        );
        send(&mut client, &ClientMessage::TerminateTerminal { request_id: 13, terminal_id: "t1".into() }).await;
        assert_eq!(next(&mut client).await, AgentMessage::Reply { request_id: 13, result: Reply::TerminalTerminated });
        send(&mut client, &ClientMessage::TerminateTerminal { request_id: 14, terminal_id: "t1".into() }).await;
        assert_eq!(
            next(&mut client).await,
            AgentMessage::Reply { request_id: 14, result: Reply::Error { message: "unknown terminal_id".into() } }
        );
    }

    #[tokio::test]
    async fn test_a_file_request_is_answered_with_its_request_id() {
        let addr = serve_agent().await;
        let mut client = connect_ready(addr).await;
        let dir = std::env::temp_dir().join(format!("smelt-agent-test-{}", std::process::id()));
        let path = dir.join("note.txt").to_string_lossy().into_owned();
        send(
            &mut client,
            &ClientMessage::WriteFile { request_id: 21, path: path.clone(), content: "a\nb\n".into(), expected_hash: None },
        )
        .await;
        let AgentMessage::Reply { request_id: 21, result: Reply::FileWritten { hash } } = next(&mut client).await else {
            panic!("write_file wasn't answered with file_written");
        };
        send(&mut client, &ClientMessage::ReadFile { request_id: 22, path: path.clone(), offset: 0, limit: 10 }).await;
        let AgentMessage::Reply { request_id: 22, result: Reply::FileRead(contents) } = next(&mut client).await else {
            panic!("read_file wasn't answered with file_read");
        };
        assert_eq!(contents.lines, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(contents.hash, hash);
        send(&mut client, &ClientMessage::ReadFile { request_id: 23, path: format!("{path}.missing"), offset: 0, limit: 10 }).await;
        assert!(matches!(
            next(&mut client).await,
            AgentMessage::Reply { request_id: 23, result: Reply::Error { .. } }
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_a_command_streams_its_output_and_exit() {
        let addr = serve_agent().await;
        let mut client = connect_ready(addr).await;
        create_terminal(&mut client, "t1").await;
        // stdout and stderr are read concurrently, so the sleep keeps the
        // stderr line ahead of the exit marker on stdout.
        run(&mut client, "t1", "c1", "echo err >&2; sleep 0.2; echo out; exit_code() { return 3; }; exit_code").await;
        let mut seen = Vec::new();
        loop {
            match next(&mut client).await {
                AgentMessage::Output { id, stream, data, .. } => {
                    assert_eq!(id, "c1");
                    seen.push((stream, data));
                }
                AgentMessage::Exit { id, code, .. } => {
                    assert_eq!((id.as_str(), code), ("c1", 3));
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        seen.sort_by_key(|(stream, _)| stream.as_str());
        assert_eq!(seen, vec![(Stream::Stderr, "err".to_string()), (Stream::Stdout, "out".to_string())]);
    }

    #[tokio::test]
    async fn test_an_unparseable_message_gets_a_protocol_error_with_its_request_id() {
        let addr = serve_agent().await;
        let mut client = connect_ready(addr).await;
        send_raw(&mut client, r#"{"action":"read_file","request_id":42}"#).await;
        let AgentMessage::ProtocolError { request_id, message } = next(&mut client).await else {
            panic!("expected a protocol_error");
        };
        assert_eq!(request_id, Some(42));
        assert!(message.contains("path"), "the error should name what's wrong: {message}");

        send_raw(&mut client, "not json at all").await;
        let AgentMessage::ProtocolError { request_id, .. } = next(&mut client).await else {
            panic!("expected a protocol_error");
        };
        assert_eq!(request_id, None);
    }

    /// A `std::io::Write` into a shared buffer, for capturing log lines.
    #[derive(Clone, Default)]
    struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log buffer").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Every agent log line from every test in this binary. One process-wide
    /// subscriber: a per-thread one (`set_default`) misses events whose
    /// callsite another thread registered first, and tests run in parallel.
    fn captured_logs() -> &'static LogBuffer {
        static LOGS: std::sync::OnceLock<LogBuffer> = std::sync::OnceLock::new();
        LOGS.get_or_init(|| {
            let logs = LogBuffer::default();
            let writer = logs.clone();
            // The agent's own logs only: tungstenite's trace logs in the
            // test's client dump every frame it sends.
            tracing_subscriber::fmt()
                .with_writer(move || writer.clone())
                .with_env_filter(tracing_subscriber::EnvFilter::new("sandbox_agent=trace"))
                .init();
            logs
        })
    }

    /// A `write_file` carries a whole file, which may hold secrets. When it
    /// can't be parsed, the log says why and how long it was, not what it
    /// said.
    #[tokio::test]
    async fn test_an_unparseable_message_is_not_logged_verbatim() {
        let logs = captured_logs();

        let addr = serve_agent().await;
        let mut client = connect_ready(addr).await;
        send_raw(
            &mut client,
            r#"{"action":"write_file","request_id":5,"path":"/tmp/x","content":"password=hunter2-SECRET","expected_hash":7}"#,
        )
        .await;
        assert!(matches!(next(&mut client).await, AgentMessage::ProtocolError { request_id: Some(5), .. }));

        let logged = String::from_utf8_lossy(&logs.0.lock().expect("log buffer")).into_owned();
        assert!(logged.contains("unparseable"), "nothing was logged: {logged:?}");
        assert!(!logged.contains("hunter2-SECRET"), "the message's content was logged: {logged}");
    }

    #[tokio::test]
    async fn test_a_second_connection_replaces_the_first() {
        let addr = serve_agent().await;
        let mut first = connect_ready(addr).await;
        create_terminal(&mut first, "t1").await;

        let mut second = connect_ready(addr).await;
        assert!(closed(&mut first).await, "the first connection is still open");

        run(&mut second, "t1", "c1", "echo hi").await;
        assert!(matches!(next(&mut second).await, AgentMessage::Output { data, .. } if data == "hi"));
        assert!(matches!(next(&mut second).await, AgentMessage::Exit { code: 0, .. }));
    }

    /// The exit of a command that finishes while smelt isn't connected (a
    /// smelt restart) reaches the next connection.
    #[tokio::test]
    async fn test_events_queued_with_no_connection_reach_the_next_one() {
        let addr = serve_agent().await;
        let mut first = connect_ready(addr).await;
        create_terminal(&mut first, "t1").await;
        run(&mut first, "t1", "c1", "sleep 0.5; echo later").await;
        first.close(None).await.expect("close");
        drop(first);
        tokio::time::sleep(Duration::from_secs(1)).await;

        let mut second = connect_ready(addr).await;
        assert!(matches!(next(&mut second).await, AgentMessage::Output { data, .. } if data == "later"));
        assert!(matches!(next(&mut second).await, AgentMessage::Exit { id, code: 0, .. } if id == "c1"));
    }

    /// A failing `accept()` (EMFILE, say) waits before trying again, instead
    /// of spinning, and the loop keeps accepting once it recovers.
    #[tokio::test(start_paused = true)]
    async fn test_the_accept_loop_backs_off_after_a_failed_accept() {
        let start = tokio::time::Instant::now();
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let (handled_tx, handled_rx) = tokio::sync::oneshot::channel();
        let mut handled_tx = Some(handled_tx);
        let counter = calls.clone();
        tokio::spawn(accept_loop(
            move || {
                let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if n < 5 {
                        Err(std::io::Error::other("too many open files"))
                    } else {
                        // One connection, then nothing more to accept.
                        if n == 5 { Ok(n) } else { std::future::pending().await }
                    }
                }
            },
            move |n| {
                if let Some(tx) = handled_tx.take() {
                    let _ = tx.send((n, tokio::time::Instant::now()));
                }
            },
        ));
        let (n, at) = handled_rx.await.expect("the loop stopped accepting");
        assert_eq!(n, 5);
        assert!(
            at - start >= Duration::from_millis(5 * 50),
            "five failed accepts took {:?}, so the loop didn't wait between them",
            at - start
        );
    }
}
