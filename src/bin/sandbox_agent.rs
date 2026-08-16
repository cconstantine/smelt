//! The sandbox agent: injected into and launched inside a sandbox pod by
//! `src/sandbox.rs`, hosting N persistent named inner `bash` shells behind
//! one WebSocket server the main smelt process talks to. See
//! `docs/projects/plans/sandbox-terminal.md` for the full design — this
//! file implements the "Protocol" and the `sandbox_agent.rs` bullet in
//! "Which files" exactly: one agent process per pod, multiplexing every
//! terminal that pod hosts (`HashMap<terminal_id, Shell>`), each shell its
//! own process group (`.process_group(0)`) so terminating one never
//! touches the agent or any sibling terminal. Per-shell: the command
//! framing is unchanged, foreground `eval` (backgrounding it broke
//! `cd`/`export` persistence — see the plan's "How"); `set -m` + `trap ':'
//! INT` at startup are both required, for different reasons, also covered
//! there; PID/process-group discovery for `send_signal` reads
//! `/proc/<bash_pid>/task/<bash_pid>/children` reactively, not eagerly.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
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
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex as AsyncMutex, mpsc};

const LISTEN_ADDR: &str = "0.0.0.0:8088";
const PID_FILE: &str = "/tmp/sandbox_agent.pid";
const MARKER_PREFIX: &str = "MARKER:";
/// Bounded retry for the one real race left once PID discovery moved from
/// eager (right after spawn) to reactive (only when `send_signal` is
/// called): a `send_signal` arriving essentially back-to-back with the
/// command that started it, before bash has necessarily finished forking.
/// Not measured — see the plan's Open Questions.
const SIGNAL_DISCOVERY_RETRIES: u32 = 10;
const SIGNAL_DISCOVERY_INTERVAL: Duration = Duration::from_millis(10);

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// SHA-256 hex digest of a file's full content — what `read_file` returns
/// alongside its (possibly paginated) slice, and what `edit_file`/
/// `write_file` compare an `expected_hash` against before writing. See
/// docs/projects/plans/file-tools.md's "Change detection, not just 'was it
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
    1 + text.as_bytes()[..offset].iter().filter(|&&b| b == b'\n').count() as u32
}

/// `edit_file`'s core replace logic — see
/// docs/projects/plans/file-tools.md's "What"/"How" on `edit_file`.
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
        return Err(EditError::Ambiguous { count: matches.len() });
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

/// One `list_directory` entry — `size` is only meaningful for a file (a
/// directory's byte size on disk isn't what a caller of this tool wants to
/// know), see docs/projects/plans/file-tools.md's "What."
#[derive(Debug, Clone, PartialEq, Serialize)]
struct DirEntryInfo {
    name: String,
    is_dir: bool,
    size: Option<u64>,
}

/// Alphabetical by name, case-sensitive (Rust's default `str` ordering —
/// uppercase sorts before lowercase) — `list_directory` doesn't group
/// directories first, just a plain sort, see the plan's "What."
fn sort_entries(mut entries: Vec<DirEntryInfo>) -> Vec<DirEntryInfo> {
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// What `edit_file` reports back to the model for each `EditError` variant
/// — the structured error the plan's "Which files" bullet calls for (hash
/// mismatch / not found / ambiguous with match count / no match at the
/// given line).
fn edit_error_message(err: EditError) -> String {
    match err {
        EditError::NotFound => "old_string not found in file".to_string(),
        EditError::Ambiguous { count } => {
            format!("old_string matches {count} times; add more surrounding context, set replace_all, or set expected_line to target one occurrence")
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
        stream: &'static str,
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
                                    stream: "stdout",
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
                                stream: "stderr",
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
    /// number — see `terminate_terminal` and the plan's "Per-terminal
    /// process groups."
    bash_pid: u32,
    /// The one in-flight command's id in *this* terminal, if any — the
    /// single source of truth the single-command-in-flight-per-terminal
    /// design relies on. The *primary* enforcement of "only one at a time"
    /// is server-side (before a command is ever sent here at all — see the
    /// plan's `sandbox.rs` bullet); this is the agent's own defensive
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
}

#[derive(Deserialize)]
#[serde(tag = "action")]
enum ClientMessage {
    #[serde(rename = "create_terminal")]
    CreateTerminal { terminal_id: String },
    #[serde(rename = "terminate_terminal")]
    TerminateTerminal { terminal_id: String },
    #[serde(rename = "command")]
    Command { terminal_id: String, id: String, command: String },
    #[serde(rename = "signal")]
    Signal { terminal_id: String, id: String, signal: String },
    #[serde(rename = "read_file")]
    ReadFile { request_id: String, path: String, offset: u32, limit: u32 },
    #[serde(rename = "write_file")]
    WriteFile { request_id: String, path: String, content: String, expected_hash: Option<String> },
    #[serde(rename = "edit_file")]
    EditFile {
        request_id: String,
        path: String,
        old_string: String,
        new_string: String,
        replace_all: bool,
        expected_hash: String,
        expected_line: Option<u32>,
    },
    #[serde(rename = "list_directory")]
    ListDirectory { request_id: String, path: String },
}

#[derive(Serialize)]
#[serde(untagged)]
enum ServerMessage {
    Line {
        id: String,
        terminal_id: String,
        stream: &'static str,
        seq: u64,
        data: String,
    },
    Exit {
        id: String,
        terminal_id: String,
        event: &'static str,
        code: i32,
    },
    TerminalCreated {
        terminal_id: String,
        event: &'static str,
    },
    TerminalTerminated {
        terminal_id: String,
        event: &'static str,
    },
    TerminalError {
        terminal_id: String,
        event: &'static str,
        message: String,
    },
    FileRead {
        request_id: String,
        event: &'static str,
        lines: Vec<String>,
        total_lines: usize,
        hash: String,
    },
    FileWritten {
        request_id: String,
        event: &'static str,
        hash: String,
    },
    FileEdited {
        request_id: String,
        event: &'static str,
        hash: String,
    },
    DirectoryListed {
        request_id: String,
        event: &'static str,
        entries: Vec<DirEntryInfo>,
    },
    FileError {
        request_id: String,
        event: &'static str,
        message: String,
    },
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    loop {
        tokio::select! {
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
                        send_server_message(&mut socket, ServerMessage::Line { id, terminal_id, stream, seq, data }).await;
                    }
                    Some(ShellEvent::Marker { terminal_id, exit_code }) => {
                        let shell = state.terminals.lock().await.get(&terminal_id).cloned();
                        let Some(shell) = shell else { continue };
                        let id = shell.current.lock().await.take();
                        if let Some(id) = id {
                            send_server_message(
                                &mut socket,
                                ServerMessage::Exit { id, terminal_id, event: "exit", code: exit_code },
                            )
                            .await;
                        }
                    }
                    None => break, // AppState (and its events_tx) dropped — agent shutting down
                }
            }
        }
    }
}

async fn recv_shell_event(state: &Arc<AppState>) -> Option<ShellEvent> {
    state.events_rx.lock().await.recv().await
}

async fn send_server_message(socket: &mut WebSocket, message: ServerMessage) {
    let Ok(text) = serde_json::to_string(&message) else {
        return;
    };
    let _ = socket.send(Message::Text(text.into())).await;
}

async fn handle_client_message(text: &str, state: &Arc<AppState>, socket: &mut WebSocket) {
    let Ok(msg) = serde_json::from_str::<ClientMessage>(text) else {
        tracing::warn!(%text, "unparseable client message, ignoring");
        return;
    };

    match msg {
        ClientMessage::CreateTerminal { terminal_id } => create_terminal(state, socket, terminal_id).await,
        ClientMessage::TerminateTerminal { terminal_id } => terminate_terminal(state, socket, terminal_id).await,
        ClientMessage::Command { terminal_id, id, command } => {
            start_command(state, &terminal_id, id, &command).await
        }
        ClientMessage::Signal { terminal_id, id, signal } => {
            signal_current_command(state, &terminal_id, &id, &signal).await
        }
        ClientMessage::ReadFile { request_id, path, offset, limit } => {
            handle_read_file(socket, request_id, path, offset, limit).await
        }
        ClientMessage::WriteFile { request_id, path, content, expected_hash } => {
            handle_write_file(socket, request_id, path, content, expected_hash).await
        }
        ClientMessage::EditFile { request_id, path, old_string, new_string, replace_all, expected_hash, expected_line } => {
            handle_edit_file(socket, request_id, path, old_string, new_string, replace_all, expected_hash, expected_line).await
        }
        ClientMessage::ListDirectory { request_id, path } => {
            handle_list_directory(socket, request_id, path).await
        }
    }
}

/// Shared across `read_file`/`edit_file`/`write_file` — an unbounded file
/// becomes an unbounded tool-result message persisted into Postgres and
/// re-sent on every subsequent turn, see
/// docs/projects/plans/file-tools.md's "Size bound."
const MAX_FILE_SIZE_BYTES: u64 = 256 * 1024;
/// `list_directory`'s analogous bound, as an entry count rather than bytes
/// — same reasoning, see the plan's "Size bound."
const MAX_DIR_ENTRIES: usize = 1000;

async fn send_file_error(socket: &mut WebSocket, request_id: String, message: &str) {
    send_server_message(
        socket,
        ServerMessage::FileError { request_id, event: "file_error", message: message.to_string() },
    )
    .await;
}

async fn handle_read_file(socket: &mut WebSocket, request_id: String, path: String, offset: u32, limit: u32) {
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(e) => {
            send_file_error(socket, request_id, &format!("failed to read {path}: {e}")).await;
            return;
        }
    };
    if bytes.len() as u64 > MAX_FILE_SIZE_BYTES {
        send_file_error(socket, request_id, &format!("{path} exceeds the {MAX_FILE_SIZE_BYTES}-byte size limit")).await;
        return;
    }
    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(_) => {
            send_file_error(socket, request_id, &format!("{path} is not valid UTF-8")).await;
            return;
        }
    };
    let hash = hash_content(content.as_bytes());
    let (lines, total_lines) = paginate_lines(&content, offset, limit);
    send_server_message(socket, ServerMessage::FileRead { request_id, event: "file_read", lines, total_lines, hash }).await;
}

async fn handle_write_file(
    socket: &mut WebSocket,
    request_id: String,
    path: String,
    content: String,
    expected_hash: Option<String>,
) {
    if content.len() as u64 > MAX_FILE_SIZE_BYTES {
        send_file_error(socket, request_id, &format!("content exceeds the {MAX_FILE_SIZE_BYTES}-byte size limit")).await;
        return;
    }

    if let Some(expected) = &expected_hash {
        match tokio::fs::read(&path).await {
            Ok(current) => {
                let current_hash = hash_content(&current);
                if &current_hash != expected {
                    send_file_error(
                        socket,
                        request_id,
                        &format!("{path} has changed since it was last read (expected hash {expected}, found {current_hash}); read_file again before writing"),
                    )
                    .await;
                    return;
                }
            }
            Err(e) => {
                send_file_error(socket, request_id, &format!("failed to read {path} for hash check: {e}")).await;
                return;
            }
        }
    }

    if let Some(parent) = std::path::Path::new(&path).parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                send_file_error(socket, request_id, &format!("failed to create parent directories for {path}: {e}")).await;
                return;
            }
        }
    }

    if let Err(e) = tokio::fs::write(&path, content.as_bytes()).await {
        send_file_error(socket, request_id, &format!("failed to write {path}: {e}")).await;
        return;
    }
    let hash = hash_content(content.as_bytes());
    send_server_message(socket, ServerMessage::FileWritten { request_id, event: "file_written", hash }).await;
}

async fn handle_edit_file(
    socket: &mut WebSocket,
    request_id: String,
    path: String,
    old_string: String,
    new_string: String,
    replace_all: bool,
    expected_hash: String,
    expected_line: Option<u32>,
) {
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(e) => {
            send_file_error(socket, request_id, &format!("failed to read {path}: {e}")).await;
            return;
        }
    };
    let current_hash = hash_content(&bytes);
    if current_hash != expected_hash {
        send_file_error(
            socket,
            request_id,
            &format!("{path} has changed since it was last read (expected hash {expected_hash}, found {current_hash}); read_file again before editing"),
        )
        .await;
        return;
    }
    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(_) => {
            send_file_error(socket, request_id, &format!("{path} is not valid UTF-8")).await;
            return;
        }
    };

    let new_content = match apply_edit(&content, &old_string, &new_string, replace_all, expected_line) {
        Ok(new_content) => new_content,
        Err(edit_err) => {
            send_file_error(socket, request_id, &edit_error_message(edit_err)).await;
            return;
        }
    };

    if let Err(e) = tokio::fs::write(&path, new_content.as_bytes()).await {
        send_file_error(socket, request_id, &format!("failed to write {path}: {e}")).await;
        return;
    }
    let hash = hash_content(new_content.as_bytes());
    send_server_message(socket, ServerMessage::FileEdited { request_id, event: "file_edited", hash }).await;
}

async fn handle_list_directory(socket: &mut WebSocket, request_id: String, path: String) {
    let mut read_dir = match tokio::fs::read_dir(&path).await {
        Ok(read_dir) => read_dir,
        Err(e) => {
            send_file_error(socket, request_id, &format!("failed to read directory {path}: {e}")).await;
            return;
        }
    };

    let mut entries = Vec::new();
    loop {
        match read_dir.next_entry().await {
            Ok(Some(entry)) => {
                if entries.len() >= MAX_DIR_ENTRIES {
                    send_file_error(socket, request_id, &format!("{path} has more than {MAX_DIR_ENTRIES} entries")).await;
                    return;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                let size = if is_dir { None } else { entry.metadata().await.ok().map(|m| m.len()) };
                entries.push(DirEntryInfo { name, is_dir, size });
            }
            Ok(None) => break,
            Err(e) => {
                send_file_error(socket, request_id, &format!("failed to read directory {path}: {e}")).await;
                return;
            }
        }
    }

    let entries = sort_entries(entries);
    send_server_message(socket, ServerMessage::DirectoryListed { request_id, event: "directory_listed", entries }).await;
}

/// Spawns a new named shell — `.process_group(0)` gives it a process group
/// distinct from the agent's and from every sibling terminal's (see the
/// plan's "Per-terminal process groups"), so `terminate_terminal` can
/// `killpg` it in isolation later. `set -m`/`trap ':' INT` are the same
/// per-shell startup this design has always used, just no longer only at
/// agent-launch time.
async fn create_terminal(state: &Arc<AppState>, socket: &mut WebSocket, terminal_id: String) {
    if state.terminals.lock().await.contains_key(&terminal_id) {
        send_terminal_error(socket, terminal_id, "terminal_id already exists").await;
        return;
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
            send_terminal_error(socket, terminal_id, &format!("failed to spawn shell: {e}")).await;
            return;
        }
    };
    let bash_pid = child.id().expect("bash should have a pid immediately after spawn");
    let mut stdin = child.stdin.take().expect("stdin requested via Stdio::piped()");
    let stdout = BufReader::new(child.stdout.take().expect("stdout requested via Stdio::piped()"));
    let stderr = BufReader::new(child.stderr.take().expect("stderr requested via Stdio::piped()"));

    // set -m: job control, so every command (including a plain foreground
    // one) gets its own process group send_signal can target.
    // trap ':' INT: without this, a non-interactive bash re-raises SIGINT
    // against itself once a foreground job dies from it, taking the whole
    // shell down with it — see the plan's "Signaling a running command."
    if let Err(e) = stdin.write_all(b"set -m\ntrap ':' INT\n").await {
        send_terminal_error(socket, terminal_id, &format!("failed to initialize shell: {e}")).await;
        return;
    }
    let _ = stdin.flush().await;

    tokio::spawn(run_reader(terminal_id.clone(), stdout, stderr, state.events_tx.clone()));

    let shell = Arc::new(Shell {
        stdin: AsyncMutex::new(stdin),
        bash_pid,
        current: AsyncMutex::new(None),
        _bash_child: AsyncMutex::new(child),
    });
    state.terminals.lock().await.insert(terminal_id.clone(), shell);

    send_server_message(
        socket,
        ServerMessage::TerminalCreated { terminal_id, event: "terminal_created" },
    )
    .await;
}

/// `killpg` targeting just this terminal's own process group — safe in
/// isolation because `.process_group(0)` gave it one distinct from the
/// agent's and every sibling terminal's. See the plan's "Terminating a
/// terminal without touching the pod, or its siblings."
async fn terminate_terminal(state: &Arc<AppState>, socket: &mut WebSocket, terminal_id: String) {
    let shell = state.terminals.lock().await.remove(&terminal_id);
    let Some(shell) = shell else {
        send_terminal_error(socket, terminal_id, "unknown terminal_id").await;
        return;
    };

    if let Err(err) = signal::kill(Pid::from_raw(-(shell.bash_pid as i32)), Signal::SIGKILL) {
        tracing::warn!(%err, %terminal_id, "killpg failed while terminating terminal");
    }

    send_server_message(
        socket,
        ServerMessage::TerminalTerminated { terminal_id, event: "terminal_terminated" },
    )
    .await;
}

async fn send_terminal_error(socket: &mut WebSocket, terminal_id: String, message: &str) {
    send_server_message(
        socket,
        ServerMessage::TerminalError { terminal_id, event: "terminal_error", message: message.to_string() },
    )
    .await;
}

async fn start_command(state: &Arc<AppState>, terminal_id: &str, id: String, command: &str) {
    let Some(shell) = state.terminals.lock().await.get(terminal_id).cloned() else {
        tracing::warn!(%terminal_id, %id, "rejecting command: unknown terminal_id");
        return;
    };

    let mut current = shell.current.lock().await;
    if current.is_some() {
        // Should already be prevented server-side (see the plan) — this is
        // a defensive backstop, not the primary enforcement, so a quiet
        // log rather than inventing a new protocol error message.
        tracing::warn!(%id, "rejecting command: another is already in flight in this terminal");
        return;
    }
    *current = Some(id);
    drop(current);

    let payload = format!("eval {}\necho \"{MARKER_PREFIX}$?\"\n", shell_quote(command));
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
/// See the plan's "Discovering a running command's process group."
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let result = apply_edit("hello world", "world", "there", false, None).expect("should succeed");
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
        let result =
            apply_edit(content, "let x = 1;", "let x = 2;", false, Some(expected_line)).expect("should succeed");
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
        assert_eq!(total, 3, "total should reflect the whole file, not just the returned slice");
    }

    #[test]
    fn test_paginate_lines_offset_beyond_end_returns_empty_but_correct_total() {
        let (lines, total) = paginate_lines("a\nb\nc", 99, 10);
        assert!(lines.is_empty());
        assert_eq!(total, 3);
    }

    fn entry(name: &str, is_dir: bool, size: Option<u64>) -> DirEntryInfo {
        DirEntryInfo { name: name.to_string(), is_dir, size }
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
        assert_eq!(edit_error_message(EditError::NotFound), "old_string not found in file");
    }

    #[test]
    fn test_edit_error_message_ambiguous_includes_match_count() {
        let message = edit_error_message(EditError::Ambiguous { count: 4 });
        assert!(message.contains('4'), "expected the match count in the message, got: {message}");
    }

    #[test]
    fn test_edit_error_message_no_match_at_line_includes_the_line_number() {
        let message = edit_error_message(EditError::NoMatchAtLine { line: 42 });
        assert!(message.contains("42"), "expected the line number in the message, got: {message}");
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    std::fs::write(PID_FILE, std::process::id().to_string())
        .unwrap_or_else(|e| panic!("failed to write {PID_FILE}: {e}"));

    // This process is launched (via `pods.exec`) as a descendant of the
    // pod's PID 1 (`sleep infinity`), which never installs a SIGINT/SIGQUIT
    // handler — the kernel's PID-1 rule then forces those to SIG_IGN, and
    // SIG_IGN (unlike a caught signal) survives exec(), so it was inherited
    // all the way down into this very process. Left alone, that ignore
    // would keep propagating into every `bash` this agent spawns and every
    // command it runs — and POSIX forbids a *non-interactive* bash from
    // overriding a signal that was already ignored "on entry", so
    // `trap ':' INT` (in `create_terminal`) would be a silent no-op and
    // `send_signal`'s SIGINT would never actually reach anything. Reset
    // both to the default disposition once, here, before spawning any
    // shell, so the ignore stops at the agent and never propagates
    // further — every terminal's `bash`, whenever it's created, inherits
    // the corrected disposition.
    unsafe {
        signal::signal(Signal::SIGINT, signal::SigHandler::SigDfl)
            .expect("reset SIGINT to default disposition");
        signal::signal(Signal::SIGQUIT, signal::SigHandler::SigDfl)
            .expect("reset SIGQUIT to default disposition");
    }

    let (tx, rx) = mpsc::unbounded_channel();
    let state = Arc::new(AppState {
        terminals: AsyncMutex::new(HashMap::new()),
        events_tx: tx,
        events_rx: AsyncMutex::new(rx),
    });

    let app = Router::new().route("/ws", get(ws_handler)).with_state(state);
    let listener = tokio::net::TcpListener::bind(LISTEN_ADDR)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {LISTEN_ADDR}: {e}"));
    tracing::info!("sandbox_agent listening on {LISTEN_ADDR}");
    axum::serve(listener, app).await.expect("server error");
}
