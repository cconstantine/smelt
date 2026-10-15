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
/// Starts every command's completion marker; `new_marker` adds the nonce.
const MARKER_PREFIX: &str = "__smelt_done__:";
/// Bounded retry for the one real race left once PID discovery moved from
/// eager (right after spawn) to reactive (only when `send_signal` is
/// called): a `send_signal` arriving essentially back-to-back with the
/// command that started it, before bash has necessarily finished forking.
/// Not measured — see SME-9's Open Questions.
const SIGNAL_DISCOVERY_RETRIES: u32 = 10;
const SIGNAL_DISCOVERY_INTERVAL: Duration = Duration::from_millis(10);

/// Finds a command's completion marker (`marker`, which ends in that
/// command's nonce and a `:`) at the end of a line, and returns the output
/// in front of it (non-empty when the command's last output had no
/// newline), which of the two markers it is (`o`, written to stdout, or
/// `e`, to stderr; review 2) and the exit code after it.
fn split_marker<'a>(line: &'a str, marker: &str) -> Option<(&'a str, char, i32)> {
    let at = line.rfind(marker)?;
    let rest = &line[at + marker.len()..];
    let (tag, code) = rest.split_once(':')?;
    let tag = match tag {
        "o" => 'o',
        "e" => 'e',
        _ => return None,
    };
    Some((&line[..at], tag, code.parse().ok()?))
}

/// The completion marker for one command: `MARKER_PREFIX`, a nonce no
/// output can predict, and `:`. Unique per command (a counter) and per
/// agent start (pid and clock), hashed so it reads as one opaque token.
fn new_marker() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    use sha2::{Digest, Sha256};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let seed = format!("{}:{}:{nanos}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
    let digest = Sha256::digest(seed.as_bytes());
    let nonce: String = digest.iter().take(12).map(|b| format!("{b:02x}")).collect();
    format!("{MARKER_PREFIX}{nonce}:")
}

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
/// Each names its command (`id`, from the shell it came from), not left to
/// be looked up by terminal when it's sent: by then the terminal may have
/// a new shell, running another command (review 1).
enum ShellEvent {
    Line {
        terminal_id: String,
        /// The command in flight, or `""` for output between commands.
        id: String,
        stream: Stream,
        seq: u64,
        data: String,
    },
    Marker {
        terminal_id: String,
        /// The command that ended; `None` when none was in flight.
        id: Option<String>,
        exit_code: i32,
    },
}

/// Reads one terminal's stdout and stderr concurrently for as long as its
/// shell lives, filtering the in-flight command's completion marker
/// (`marker`, set by `start_command`) before anything reaches a client.
/// The marker is matched at the end of a line, not only as a whole line:
/// a command whose last output has no newline (`printf abc`, `head -c`)
/// has the marker glued onto that output (SME-76). Lines come from
/// `fill_piece` into a buffer kept across calls, which is cancel safe;
/// `read_line` in this `select!` dropped half a stdout line whenever stderr
/// won the race. Bytes that aren't UTF-8 are shown as U+FFFD (B2,
/// SME-122): `Lines` ended the reader on them, wedging the terminal.
/// One of these runs per terminal (spawned by `create_terminal`); while
/// idle (no command in flight) both branches simply stay pending.
async fn run_reader(
    terminal_id: String,
    mut stdout: BufReader<ChildStdout>,
    mut stderr: BufReader<ChildStderr>,
    shell: Arc<Shell>,
    tx: mpsc::UnboundedSender<ShellEvent>,
) {
    let marker = shell.marker.clone();
    let (mut stdout_buf, mut stderr_buf) = (Vec::new(), Vec::new());
    let mut seq: u64 = 0;
    // The in-flight command's end, as the two markers arrive (B12,
    // SME-122): its exit code from stdout's, and whether stderr's has come.
    // It's sent once both have, or `STDERR_MARKER_WAIT` after stdout's (a
    // command that closed or redirected the shell's stderr sends none).
    let mut ending = Ending::default();
    // Both pipes are read to their end: the shell's last words on one
    // mustn't be lost because the other closed first.
    let (mut stdout_open, mut stderr_open) = (true, true);
    while stdout_open || stderr_open {
        tokio::select! {
            result = fill_piece(&mut stdout, &mut stdout_buf), if stdout_open => {
                match result {
                    Ok(false) | Err(_) => stdout_open = false, // bash exited or pipe error
                    Ok(true) => {
                        let line = take_piece(&mut stdout_buf);
                        let (data, meaning) = match ending.find(&marker, &line) {
                            Some((before, meaning)) => (before, Some(meaning)),
                            None => (line, None),
                        };
                        if meaning.is_none() || !data.is_empty() {
                            seq += 1;
                            let id = shell.current.lock().await.clone().unwrap_or_default();
                            if tx
                                .send(ShellEvent::Line {
                                    terminal_id: terminal_id.clone(),
                                    id,
                                    stream: Stream::Stdout,
                                    seq,
                                    data,
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                        if let Some(meaning) = &meaning {
                            ending.note(meaning);
                        }
                    }
                }
            }
            result = fill_piece(&mut stderr, &mut stderr_buf), if stderr_open => {
                match result {
                    Ok(false) | Err(_) => stderr_open = false,
                    Ok(true) => {
                        let line = take_piece(&mut stderr_buf);
                        let (data, found) = match ending.find(&marker, &line) {
                            Some((before, meaning)) => (before, Some(meaning)),
                            None => (line, None),
                        };
                        if found.is_none() || !data.is_empty() {
                            seq += 1;
                            let id = shell.current.lock().await.clone().unwrap_or_default();
                            if tx
                                .send(ShellEvent::Line {
                                    terminal_id: terminal_id.clone(),
                                    id,
                                    stream: Stream::Stderr,
                                    seq,
                                    data,
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                        if let Some(meaning) = &found {
                            ending.note(meaning);
                        }
                    }
                }
            }
            _ = sleep_until_or_never(ending.deadline) => {
                // One marker came and the other didn't in time.
                ending.give_up_waiting();
            }
        }
        if let Some(exit_code) = ending.complete(&marker) {
            let id = shell.current.lock().await.take();
            if tx.send(ShellEvent::Marker { terminal_id: terminal_id.clone(), id, exit_code }).is_err() {
                return;
            }
            seq = 0;
        }
    }
    // Ended between the two markers: the command did finish.
    if let Some(exit_code) = ending.code.take() {
        marker.lock().unwrap_or_else(|e| e.into_inner()).take();
        let id = shell.current.lock().await.take();
        if tx.send(ShellEvent::Marker { terminal_id: terminal_id.clone(), id, exit_code }).is_err() {
            return;
        }
        seq = 0;
    }
    finish_ended_shell(&terminal_id, &shell, seq, &tx).await;
}

/// The most bytes of a line read into one `Output` (B13, SME-122); bytes
/// that aren't UTF-8 decode to U+FFFD's three, so a piece sent is at most
/// three times this.
const MAX_OUTPUT_CHUNK: usize = 64 * 1024;

/// How much of a long line's end `take_piece` holds back for the next
/// piece: more than a marker and its exit code (`new_marker`: 40 bytes,
/// plus up to 11), so one is never split across two pieces.
const MARKER_TAIL: usize = 64;

/// How long after a command's stdout marker its stderr marker may take
/// before the command is finished without it (B12, SME-122).
const STDERR_MARKER_WAIT: Duration = Duration::from_secs(1);

/// Where the in-flight command's end has got to: see `run_reader`. Each of
/// its two markers (`o`, written to the shell's stdout, `e` to its stderr)
/// carries the exit code and says which it is, whichever stream it arrives
/// on: a command can send the shell's stdout or stderr elsewhere, or swap
/// them (review 2).
#[derive(Default)]
struct Ending {
    /// The exit code, from whichever marker came first.
    code: Option<i32>,
    /// Whether stdout's marker has come (or been given up on).
    out_seen: bool,
    /// Whether stderr's marker has come (or been given up on).
    err_seen: bool,
    /// When to stop waiting for the other marker, once one has come.
    deadline: Option<tokio::time::Instant>,
    /// The last finished command's marker: one of its markers that comes
    /// late is dropped, not shown as the next command's output.
    previous: Option<String>,
}

impl Ending {
    /// The output before a marker at the end of `line`, on either stream,
    /// and what the marker stands for: the in-flight command's stdout or
    /// stderr marker, or, for the last command's turning up late after
    /// `complete` gave up on it, nothing (review 1: counting it as the
    /// current command's would let that one end early, and so on down the
    /// shell).
    fn find(&self, marker: &std::sync::Mutex<Option<String>>, line: &str) -> Option<(String, EndMarker)> {
        let current = marker.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some((before, tag, code)) = current.as_deref().and_then(|m| split_marker(line, m)) {
            let meaning = if tag == 'o' { EndMarker::Out(code) } else { EndMarker::Err(code) };
            return Some((before.to_string(), meaning));
        }
        let (before, _, _) = self.previous.as_deref().and_then(|m| split_marker(line, m))?;
        Some((before.to_string(), EndMarker::Stale))
    }

    /// Notes a marker: the first one gives the exit code and starts the
    /// wait for the other.
    fn note(&mut self, meaning: &EndMarker) {
        let (seen, code) = match meaning {
            EndMarker::Out(code) => (&mut self.out_seen, *code),
            EndMarker::Err(code) => (&mut self.err_seen, *code),
            EndMarker::Stale => return,
        };
        *seen = true;
        self.code.get_or_insert(code);
        self.deadline.get_or_insert_with(|| tokio::time::Instant::now() + STDERR_MARKER_WAIT);
    }

    /// The other marker didn't come in time (its stream was closed or sent
    /// elsewhere).
    fn give_up_waiting(&mut self) {
        self.out_seen = true;
        self.err_seen = true;
    }

    /// The exit code, once both markers have come: clears the in-flight
    /// marker and starts over for the next command.
    fn complete(&mut self, marker: &std::sync::Mutex<Option<String>>) -> Option<i32> {
        if !(self.out_seen && self.err_seen) {
            return None;
        }
        let code = self.code.take()?;
        self.previous = marker.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.out_seen = false;
        self.err_seen = false;
        self.deadline = None;
        Some(code)
    }
}

/// What an end marker found by `Ending::find` stands for.
#[derive(Debug, PartialEq)]
enum EndMarker {
    /// The command's stdout marker, with its exit code.
    Out(i32),
    /// The command's stderr marker, with its exit code.
    Err(i32),
    /// Nothing: dropped from the output and otherwise ignored.
    Stale,
}

/// Sleeps until `deadline`, or forever when there's none.
async fn sleep_until_or_never(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// After a terminal's shell has ended (B11, SME-122): `exit`, `exec`, a
/// `set -e` and a failure, or `terminate_terminal`'s kill. Marks it ended,
/// so the terminal's next command starts a new shell (`start_command`),
/// and finishes a command still in flight with the shell's own status and
/// a line saying what happened; before, it stayed running for good.
async fn finish_ended_shell(terminal_id: &str, shell: &Shell, seq: u64, tx: &mpsc::UnboundedSender<ShellEvent>) {
    let code = shell_exit_code(shell).await;
    // All under `current`'s lock, which `start_command` claims a command
    // under: a command claimed before this has its marker seen and gets its
    // exit here, and one after it sees `ended` and goes to a new shell
    // (review 2). The id is taken under the same lock, so a new shell's
    // command can't be the one this exit goes to.
    let mut current = shell.current.lock().await;
    shell.ended.store(true, Ordering::SeqCst);
    let in_flight = shell.marker.lock().unwrap_or_else(|e| e.into_inner()).take().is_some();
    let id = current.take();
    drop(current);
    if in_flight {
        let _ = tx.send(ShellEvent::Line {
            terminal_id: terminal_id.to_string(),
            id: id.clone().unwrap_or_default(),
            stream: Stream::Stderr,
            seq: seq + 1,
            data: format!(
                "smelt: this terminal's shell exited (status {code}); its next command runs in a new shell, with the working directory and variables reset"
            ),
        });
        let _ = tx.send(ShellEvent::Marker { terminal_id: terminal_id.to_string(), id, exit_code: code });
    }
}

/// How long an ended shell's process has to be reaped once both its pipes
/// have closed, before its process group is killed.
const SHELL_EXIT_WAIT: Duration = Duration::from_secs(5);

/// The ended shell's exit status as a shell reports one: its code, or 128
/// plus the signal that killed it. A shell that closed its pipes but
/// hasn't exited is killed first.
async fn shell_exit_code(shell: &Shell) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    let mut child = shell.bash_child.lock().await;
    let status = match tokio::time::timeout(SHELL_EXIT_WAIT, child.wait()).await {
        Ok(status) => status,
        Err(_) => {
            let _ = signal::kill(Pid::from_raw(-(shell.bash_pid as i32)), Signal::SIGKILL);
            child.wait().await
        }
    };
    match status {
        Ok(status) => status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
        Err(_) => 255,
    }
}

/// Reads into `buf` until it holds a whole line or `MAX_OUTPUT_CHUNK`
/// bytes (B13, SME-122), or the stream ends. True when there's a piece to
/// take (`take_piece`); false at the end of the stream with nothing left.
/// Cancel safe: `fill_buf` is, and what's read is moved into `buf`, kept
/// across calls, before the next await.
async fn fill_piece<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R, buf: &mut Vec<u8>) -> std::io::Result<bool> {
    loop {
        if buf.last() == Some(&b'\n') || buf.len() >= MAX_OUTPUT_CHUNK {
            return Ok(true);
        }
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(!buf.is_empty());
        }
        let room = (MAX_OUTPUT_CHUNK - buf.len()).min(available.len());
        let take = available[..room].iter().position(|&b| b == b'\n').map_or(room, |at| at + 1);
        buf.extend_from_slice(&available[..take]);
        reader.consume(take);
    }
}

/// The piece `fill_piece` read, decoded lossily (B2): a whole line without
/// its `\n` or `\r\n` (as `Lines` gave it), or the start of a long one.
/// Of a long line it keeps the last `MARKER_TAIL` bytes in `buf` for the
/// next piece, so a marker at its end is found whole, and splits it before
/// a character's first byte, never inside one (bytes that aren't UTF-8 are
/// cut where they fall). Each piece taken is never empty.
fn take_piece(buf: &mut Vec<u8>) -> String {
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    } else if buf.len() >= MAX_OUTPUT_CHUNK {
        let cut = buf.len() - MARKER_TAIL;
        // A character is at most 4 bytes, so its start is at most 3 back.
        // Bytes that aren't UTF-8 (a run of continuation bytes) have none:
        // they're cut where they are, which the lossy decode copes with
        // (review 1: walking back to 0 took nothing and spun for good).
        let at = (cut.saturating_sub(3)..=cut)
            .rev()
            .find(|&i| (buf[i] & 0b1100_0000) != 0b1000_0000)
            .filter(|&i| i > 0)
            .unwrap_or(cut);
        let piece = String::from_utf8_lossy(&buf[..at]).into_owned();
        buf.drain(..at);
        return piece;
    }
    let line = String::from_utf8_lossy(buf).into_owned();
    buf.clear();
    line
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
    /// The in-flight command's completion marker (`new_marker`), shared
    /// with this terminal's `run_reader`, which clears it on a match.
    marker: Arc<std::sync::Mutex<Option<String>>>,
    /// The shell's process, waited on once it has ended (`shell_exit_code`).
    /// Not killed on drop (tokio's default) — `terminate_terminal`'s
    /// explicit `killpg` is what actually ends it.
    bash_child: AsyncMutex<Child>,
    /// Set once the shell has ended (`finish_ended_shell`): the terminal's
    /// next command starts a new one.
    ended: std::sync::atomic::AtomicBool,
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
    /// An event taken off `events_rx` whose send failed, sent by the live
    /// connection (or the next one, straight after its hello). One is
    /// enough: a connection stops taking events once a send fails.
    unsent: std::sync::Mutex<Option<AgentMessage>>,
    /// Signalled when `unsent` fills, so the live connection sends it.
    unsent_ready: tokio::sync::Notify,
}

fn new_state() -> Arc<AppState> {
    let (tx, rx) = mpsc::unbounded_channel();
    Arc::new(AppState {
        terminals: AsyncMutex::new(HashMap::new()),
        events_tx: tx,
        events_rx: AsyncMutex::new(rx),
        current: std::sync::Mutex::new(None),
        next_connection: AtomicU64::new(0),
        unsent: std::sync::Mutex::new(None),
        unsent_ready: tokio::sync::Notify::new(),
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
/// so an exit during a smelt restart still reaches it. So does one whose
/// send failed (`send_or_keep`). Not covered: a send the kernel accepted
/// into a socket smelt had already abandoned counts as sent, and is lost.
async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    let (connection, mut replaced) = state.take_over();
    send_message(&mut socket, &AgentMessage::Hello(PROTOCOL_VERSION)).await;
    if !send_unsent(&mut socket, &state).await {
        state.release(connection);
        return;
    }
    loop {
        tokio::select! {
            // Checked first, so a replaced connection takes no more events.
            biased;
            _ = &mut replaced => {
                tracing::info!(connection, "a newer connection replaced this one");
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
            // The connection this one replaced failed a send after this one
            // had opened.
            _ = state.unsent_ready.notified() => {
                if !send_unsent(&mut socket, &state).await {
                    break;
                }
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
                let Some(event) = event else {
                    break; // AppState (and its events_tx) dropped — agent shutting down
                };
                if let Some(message) = event_message(event) {
                    if !send_or_keep(&mut socket, &state, message).await {
                        break;
                    }
                }
            }
        }
    }
    state.release(connection);
}

/// The message a shell event becomes; `None` for a marker with no command
/// in flight.
fn event_message(event: ShellEvent) -> Option<AgentMessage> {
    match event {
        ShellEvent::Line { terminal_id, id, stream, seq, data } => {
            Some(AgentMessage::Output { id, terminal_id, stream, seq, data })
        }
        ShellEvent::Marker { terminal_id, id, exit_code } => {
            Some(AgentMessage::Exit { id: id?, terminal_id, code: exit_code })
        }
    }
}

/// Sends the event a failed send left in `unsent`, if there is one.
/// Returns false if that send fails too (it's kept again).
async fn send_unsent<S>(sink: &mut S, state: &AppState) -> bool
where
    S: futures_util::Sink<Message> + Unpin,
{
    let unsent = state.unsent.lock().unwrap_or_else(|e| e.into_inner()).take();
    match unsent {
        Some(unsent) => send_or_keep(sink, state, unsent).await,
        None => true,
    }
}

async fn recv_shell_event(state: &Arc<AppState>) -> Option<ShellEvent> {
    state.events_rx.lock().await.recv().await
}

/// Sends an event to smelt. If the send fails, the event is kept for the
/// next connection (`AppState::unsent`) and this returns false: the event
/// was already taken off `events_rx`, and an exit lost here would leave its
/// command running in smelt's records for good.
async fn send_or_keep<S>(sink: &mut S, state: &AppState, message: AgentMessage) -> bool
where
    S: futures_util::Sink<Message> + Unpin,
{
    use futures_util::SinkExt;
    let text = match serde_json::to_string(&message) {
        Ok(text) => text,
        Err(e) => {
            tracing::error!(%e, "couldn't serialize a message for smelt");
            return true;
        }
    };
    if sink.send(Message::Text(text.into())).await.is_ok() {
        return true;
    }
    *state.unsent.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
    // A newer connection may already be open, past its own first look.
    state.unsent_ready.notify_one();
    false
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
    let shell = match spawn_shell(&terminal_id, &state.events_tx).await {
        Ok(shell) => shell,
        Err(message) => return reply_error(&message),
    };
    state
        .terminals
        .lock()
        .await
        .insert(terminal_id.clone(), shell);

    Reply::TerminalCreated
}

/// Starts a terminal's `bash`, set up for commands, and its reader.
async fn spawn_shell(terminal_id: &str, events_tx: &mpsc::UnboundedSender<ShellEvent>) -> Result<Arc<Shell>, String> {
    let mut child = Command::new("bash")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("failed to spawn shell: {e}"))?;
    #[expect(clippy::expect_used, reason = "a child that hasn't been waited on always has its pid")]
    let bash_pid = child
        .id()
        .expect("bash should have a pid immediately after spawn");
    #[expect(clippy::expect_used, reason = "the shell was spawned with stdin piped, and this is its one take")]
    let mut stdin = child
        .stdin
        .take()
        .expect("stdin requested via Stdio::piped()");
    #[expect(clippy::expect_used, reason = "the shell was spawned with stdout piped, and this is its one take")]
    let stdout = BufReader::new(
        child
            .stdout
            .take()
            .expect("stdout requested via Stdio::piped()"),
    );
    #[expect(clippy::expect_used, reason = "the shell was spawned with stderr piped, and this is its one take")]
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
        let _ = signal::kill(Pid::from_raw(-(bash_pid as i32)), Signal::SIGKILL);
        return Err(format!("failed to initialize shell: {e}"));
    }
    let _ = stdin.flush().await;

    let shell = Arc::new(Shell {
        stdin: AsyncMutex::new(stdin),
        bash_pid,
        current: AsyncMutex::new(None),
        marker: Arc::new(std::sync::Mutex::new(None)),
        bash_child: AsyncMutex::new(child),
        ended: std::sync::atomic::AtomicBool::new(false),
    });
    tokio::spawn(run_reader(
        terminal_id.to_string(),
        stdout,
        stderr,
        shell.clone(),
        events_tx.clone(),
    ));
    Ok(shell)
}

/// The terminal's shell for its next command, starting a new one in its
/// place if it has ended (B11, SME-122). `None` for an unknown terminal;
/// `Err` with the ended shell when a new one couldn't be started.
async fn shell_for_command(state: &Arc<AppState>, terminal_id: &str) -> Option<Result<Arc<Shell>, (Arc<Shell>, String)>> {
    let mut terminals = state.terminals.lock().await;
    let shell = terminals.get(terminal_id).cloned()?;
    if !shell.ended.load(Ordering::SeqCst) {
        return Some(Ok(shell));
    }
    Some(match spawn_shell(terminal_id, &state.events_tx).await {
        Ok(new) => {
            terminals.insert(terminal_id.to_string(), new.clone());
            Ok(new)
        }
        Err(message) => Err((shell, message)),
    })
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
    let Some(shell) = shell_for_command(state, terminal_id).await else {
        tracing::warn!(%terminal_id, %id, "rejecting command: unknown terminal_id");
        return;
    };
    let shell = match shell {
        Ok(shell) => shell,
        Err((_, message)) => {
            // The command still gets an exit, so it doesn't stay running.
            let _ = state.events_tx.send(ShellEvent::Line {
                terminal_id: terminal_id.to_string(),
                id: id.clone(),
                stream: Stream::Stderr,
                seq: 1,
                data: format!("smelt: {message}"),
            });
            let _ = state.events_tx.send(ShellEvent::Marker { terminal_id: terminal_id.to_string(), id: Some(id), exit_code: 127 });
            return;
        }
    };

    let mut current = shell.current.lock().await;
    // The shell ended between `shell_for_command` and this lock: its end
    // has been handled (under this lock, `finish_ended_shell`), so a command
    // claimed now would get no exit. Go round again for a new shell
    // (review 2).
    if shell.ended.load(Ordering::SeqCst) {
        drop(current);
        return Box::pin(start_command(state, terminal_id, id, command)).await;
    }
    if current.is_some() {
        // Should already be prevented server-side (see SME-9's plan) — this is
        // a defensive backstop, not the primary enforcement, so a quiet
        // log rather than inventing a new protocol error message.
        tracing::warn!(%id, "rejecting command: another is already in flight in this terminal");
        return;
    }
    *current = Some(id);
    let marker = new_marker();
    *shell.marker.lock().unwrap_or_else(|e| e.into_inner()) = Some(marker.clone());
    drop(current);

    // stdin from /dev/null: the shell reads this payload from its own
    // stdin, so a command that reads stdin (`cat`, `read`, a prompt) would
    // otherwise swallow the marker line and never finish. Nothing writes
    // to a terminal command's stdin.
    // The marker goes to stdout with the exit code, then to stderr: the
    // command finished before either, so everything it wrote to stderr is
    // ahead of the second in its pipe (B12, SME-122).
    // Both markers carry the exit code and say which they are (`o`, `e`),
    // so either one ends the command whichever stream it arrives on
    // (review 2). The status is saved first: `$?` changes after a printf.
    let payload = format!(
        "eval {} </dev/null\n__smelt_rc=$?; printf '%so:%s\\n' {m} \"$__smelt_rc\"; printf '%se:%s\\n' {m} \"$__smelt_rc\" >&2; unset __smelt_rc\n",
        shell_quote(command),
        m = shell_quote(&marker)
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

    /// SME-122 review 1: the last command's marker turning up late (its
    /// stderr was slow past the fallback) is dropped, and doesn't count as
    /// the next command's: that one still waits for its own.
    /// Whether `found` (from `Ending::find`) stands for the in-flight
    /// command.
    fn counts_for_current(found: &Option<(String, EndMarker)>) -> bool {
        matches!(found, Some((_, EndMarker::Out(_) | EndMarker::Err(_))))
    }

    #[test]
    fn test_a_late_marker_of_the_last_command_changes_nothing() {
        let marker = std::sync::Mutex::new(Some("M_A:".to_string()));
        let mut ending = Ending::default();
        let (_, out) = ending.find(&marker, "M_A:o:0").expect("stdout's marker");
        ending.note(&out);
        ending.give_up_waiting(); // stderr's didn't come in time
        assert_eq!(ending.complete(&marker), Some(0));

        *marker.lock().unwrap_or_else(|e| e.into_inner()) = Some("M_B:".to_string());
        let late = ending.find(&marker, "M_A:e:0");
        assert!(late.is_some(), "the late marker is recognized, so it isn't shown");
        assert!(!counts_for_current(&late), "it doesn't stand for the current command's marker");
    }

    #[test]
    fn test_split_marker_finds_a_marker_alone_on_its_line() {
        assert_eq!(split_marker("__smelt_done__:n1:o:0", "__smelt_done__:n1:"), Some(("", 'o', 0)));
        assert_eq!(split_marker("__smelt_done__:n1:e:130", "__smelt_done__:n1:"), Some(("", 'e', 130)));
    }

    #[test]
    fn test_split_marker_finds_a_marker_glued_to_output_without_a_newline() {
        // Conversation 23's `head -c 400` of a JSON body.
        assert_eq!(
            split_marker(r#""conclusion":"succes__smelt_done__:n1:o:0"#, "__smelt_done__:n1:"),
            Some((r#""conclusion":"succes"#, 'o', 0))
        );
    }

    #[test]
    fn test_split_marker_ignores_another_commands_nonce_and_the_old_marker() {
        assert_eq!(split_marker("__smelt_done__:n2:o:0", "__smelt_done__:n1:"), None);
        assert_eq!(split_marker("MARKER:0", "__smelt_done__:n1:"), None);
        assert_eq!(split_marker("__smelt_done__:n1:", "__smelt_done__:n1:"), None);
        assert_eq!(split_marker("__smelt_done__:n1:o:0 trailing", "__smelt_done__:n1:"), None);
        assert_eq!(split_marker("__smelt_done__:n1:x:0", "__smelt_done__:n1:"), None);
        assert_eq!(split_marker("__smelt_done__:n1:0", "__smelt_done__:n1:"), None, "the untagged form");
    }

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
        #[expect(clippy::expect_used, reason = "startup: without its signals reset, no terminal's commands can be interrupted")]
        signal::signal(Signal::SIGINT, signal::SigHandler::SigDfl)
            .expect("reset SIGINT to default disposition");
        #[expect(clippy::expect_used, reason = "startup: without its signals reset, no terminal's commands can be interrupted")]
        signal::signal(Signal::SIGQUIT, signal::SigHandler::SigDfl)
            .expect("reset SIGQUIT to default disposition");
        // As PID 1 with no handler, SIGTERM would be ignored, and every
        // deleted pod would wait out its whole grace period before being
        // killed. The pod is going away: exit at once (SME-51 B6).
        #[expect(clippy::expect_used, reason = "startup: as PID 1, an agent that can't handle SIGTERM holds up every pod delete")]
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
    #[expect(clippy::expect_used, reason = "serving is the agent's whole job: when it ends, the pod does")]
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
        serve_agent_with(new_state()).await
    }

    async fn serve_agent_with(state: Arc<AppState>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, router(state)).await });
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
        client.send(WsMessage::Text(text.into())).await.expect("send");
    }

    async fn send_raw(client: &mut Client, text: &str) {
        client.send(WsMessage::Text(text.into())).await.expect("send");
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

    /// Runs `command` in a fresh terminal and collects its stdout lines and
    /// exit code, failing if no exit arrives within `WAIT`.
    async fn run_to_exit(command: &str) -> (Vec<String>, i32) {
        let addr = serve_agent().await;
        let mut client = connect_ready(addr).await;
        create_terminal(&mut client, "t1").await;
        run(&mut client, "t1", "c1", command).await;
        let mut stdout = Vec::new();
        loop {
            match next(&mut client).await {
                AgentMessage::Output { stream: Stream::Stdout, data, .. } => stdout.push(data),
                AgentMessage::Output { .. } => {}
                AgentMessage::Exit { id, code, .. } => {
                    assert_eq!(id, "c1");
                    return (stdout, code);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    /// Runs `command` as `id` in `terminal_id` on an open client and
    /// collects its stdout and stderr lines and exit code.
    async fn run_collect(client: &mut Client, terminal_id: &str, id: &str, command: &str) -> (Vec<String>, Vec<String>, i32) {
        run(client, terminal_id, id, command).await;
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        loop {
            match next(client).await {
                AgentMessage::Output { stream: Stream::Stdout, data, .. } => stdout.push(data),
                AgentMessage::Output { stream: Stream::Stderr, data, .. } => stderr.push(data),
                AgentMessage::Exit { id: exited, code, .. } => {
                    assert_eq!(exited, id);
                    return (stdout, stderr, code);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    /// B11 (SME-122): a command that ends its terminal's shell (`exit`,
    /// `exec`) used to stay running for good, wedging the terminal. It
    /// finishes with the shell's status and says so, and the terminal's
    /// next command runs in a new shell.
    #[tokio::test]
    async fn test_a_command_that_ends_its_shell_finishes_and_the_terminal_keeps_working() {
        let addr = serve_agent().await;
        let mut client = connect_ready(addr).await;
        create_terminal(&mut client, "t1").await;

        let (stdout, stderr, code) = run_collect(&mut client, "t1", "c1", "echo bye; exit 3").await;
        assert_eq!((stdout, code), (vec!["bye".to_string()], 3));
        assert!(
            stderr.iter().any(|line| line.contains("shell exited") && line.contains("new shell")),
            "the command's output should say the shell ended: {stderr:?}"
        );

        let (stdout, _, code) = run_collect(&mut client, "t1", "c2", "echo again").await;
        assert_eq!((stdout, code), (vec!["again".to_string()], 0));
    }

    /// B12 (SME-122): a command's stderr is all sent before its exit. The
    /// exit marker comes on stdout, so stderr still in its pipe could be
    /// read after it and lost. 2,000 lines overfill the pipe, so most of
    /// them are still unread when the marker arrives, every run.
    #[tokio::test]
    async fn test_all_of_a_commands_stderr_comes_before_its_exit() {
        let addr = serve_agent().await;
        let mut client = connect_ready(addr).await;
        create_terminal(&mut client, "t1").await;

        let (stdout, stderr, code) = run_collect(
            &mut client,
            "t1",
            "c1",
            "echo out1; for i in $(seq 1 2000); do echo \"err $i\" >&2; done; echo out2; printf tail >&2",
        )
        .await;

        assert_eq!((stdout, code), (vec!["out1".to_string(), "out2".to_string()], 0));
        let mut expected: Vec<String> = (1..=2000).map(|i| format!("err {i}")).collect();
        expected.push("tail".to_string());
        assert_eq!(stderr.len(), expected.len(), "stderr lines before the exit");
        assert_eq!(stderr, expected);

        // Nothing of c1's turns up in the next command.
        let (stdout, stderr, code) = run_collect(&mut client, "t1", "c2", "echo next").await;
        assert_eq!((stdout, stderr, code), (vec!["next".to_string()], vec![], 0));
    }

    /// SME-122 review 1: after `exec 2>&1` both end markers come on
    /// stdout. The second is the stderr marker: it isn't shown as output,
    /// and the command doesn't wait out the fallback for it.
    #[tokio::test]
    async fn test_a_shell_with_stderr_on_stdout_shows_no_marker_and_doesnt_wait() {
        let addr = serve_agent().await;
        let mut client = connect_ready(addr).await;
        create_terminal(&mut client, "t1").await;

        let (stdout, _, code) = run_collect(&mut client, "t1", "c1", "exec 2>&1; echo one").await;
        assert_eq!((stdout, code), (vec!["one".to_string()], 0));
        let started = std::time::Instant::now();
        let (stdout, _, code) = run_collect(&mut client, "t1", "c2", "echo two; echo err-two >&2").await;
        assert_eq!((stdout, code), (vec!["two".to_string(), "err-two".to_string()], 0));
        assert!(started.elapsed() < STDERR_MARKER_WAIT, "waited {:?} for a marker that came on stdout", started.elapsed());
    }

    /// SME-122 review 2: the end markers say which they are, so a command
    /// that sends the shell's stdout elsewhere (to stderr, to /dev/null, or
    /// swaps the two) still finishes, with its own exit code.
    #[tokio::test]
    async fn test_a_command_that_moves_the_shells_stdout_finishes_with_its_code() {
        let addr = serve_agent().await;
        let mut client = connect_ready(addr).await;
        for (n, (command, code)) in [
            ("exec 1>&2; echo one; false", 1),
            ("exec >/dev/null; echo gone; (exit 5)", 5),
            ("exec 3>&1 1>&2 2>&3; (exit 7)", 7),
        ]
        .into_iter()
        .enumerate()
        {
            let terminal = format!("t{n}");
            create_terminal(&mut client, &terminal).await;
            let (stdout, stderr, exited) = run_collect(&mut client, &terminal, &format!("c{n}"), command).await;
            assert_eq!(exited, code, "{command}");
            assert!(
                stdout.iter().chain(&stderr).all(|line| !line.contains(MARKER_PREFIX)),
                "{command}: a marker shown as output: {stdout:?} {stderr:?}"
            );
        }
    }

    /// B12 (SME-122): a command that sends the shell's stderr elsewhere
    /// still finishes, once the wait for stderr's marker runs out, and the
    /// terminal goes on working.
    #[tokio::test]
    async fn test_a_command_that_redirects_the_shells_stderr_still_finishes() {
        let addr = serve_agent().await;
        let mut client = connect_ready(addr).await;
        create_terminal(&mut client, "t1").await;

        let (stdout, _, code) = run_collect(&mut client, "t1", "c1", "exec 2>/dev/null; echo still").await;
        assert_eq!((stdout, code), (vec!["still".to_string()], 0));
        let (stdout, _, code) = run_collect(&mut client, "t1", "c2", "echo next").await;
        assert_eq!((stdout, code), (vec!["next".to_string()], 0));
    }

    /// B13 (SME-122): a very long line goes out in pieces no longer than
    /// `MAX_OUTPUT_CHUNK`; one 20 MB line was over smelt's WebSocket frame
    /// limit and dropped the connection. The pieces join up to the line,
    /// a multi-byte character isn't split, and a marker glued onto the end
    /// of a long line without a newline still ends the command.
    #[tokio::test]
    async fn test_a_long_line_is_sent_in_pieces_that_join_up() {
        let addr = serve_agent().await;
        let mut client = connect_ready(addr).await;
        create_terminal(&mut client, "t1").await;
        let cases = [
            ("head -c 200000 /dev/zero | tr '\\0' x; echo", "x".repeat(200_000)),
            ("head -c 200000 /dev/zero | tr '\\0' x", "x".repeat(200_000)),
            ("for i in $(seq 1 40001); do printf '\u{e9}'; done; echo", "\u{e9}".repeat(40_001)),
            // Review 1: bytes that are all UTF-8 continuation bytes (8-bit
            // audio's silence, a raw grey image) have no character start to
            // split before.
            ("head -c 70000 /dev/zero | tr '\\0' '\\200'; echo", "\u{FFFD}".repeat(70_000)),
        ];
        for (n, (command, line)) in cases.into_iter().enumerate() {
            let (stdout, _, code) = run_collect(&mut client, "t1", &format!("c{n}"), command).await;
            assert_eq!(code, 0, "{command}");
            // The cap is on bytes read; decoding turns each byte that isn't
            // UTF-8 into U+FFFD's three.
            let cap = if line.contains('\u{FFFD}') { 3 * MAX_OUTPUT_CHUNK } else { MAX_OUTPUT_CHUNK };
            assert!(
                stdout.iter().all(|piece| piece.len() <= cap),
                "{command}: pieces of {:?} bytes",
                stdout.iter().map(String::len).collect::<Vec<_>>()
            );
            assert!(stdout.len() > 1, "{command}: sent in one piece");
            assert!(stdout.concat() == line, "{command}: the pieces don't join up to the line");
        }
    }

    #[tokio::test]
    async fn test_a_command_whose_output_has_no_trailing_newline_still_exits() {
        // Conversation 23: `head -c 400` glued the marker onto the output's
        // last line, so the command never finished and the terminal wedged.
        assert_eq!(run_to_exit("printf abc").await, (vec!["abc".to_string()], 0));
    }

    #[tokio::test]
    async fn test_a_partial_stdout_line_survives_stderr_arriving_first() {
        // `read_line` isn't cancel safe: when the stderr branch of the
        // reader's `select!` won while stdout held half a line, that half
        // was dropped.
        assert_eq!(
            run_to_exit("printf abc; sleep 0.3; echo err >&2; sleep 0.3; echo def").await,
            (vec!["abcdef".to_string()], 0)
        );
    }

    /// B2 (SME-122): output that isn't UTF-8 (`cat` of a binary) used to
    /// end the reader, so the command never finished and its terminal
    /// wedged. It's shown with replacement characters instead.
    #[tokio::test]
    async fn test_output_that_isnt_utf8_still_finishes_the_command() {
        assert_eq!(
            run_to_exit("printf 'a\\xff\\xfeb\\n'; echo after").await,
            (vec!["a\u{FFFD}\u{FFFD}b".to_string(), "after".to_string()], 0)
        );
    }

    #[tokio::test]
    async fn test_a_command_that_reads_stdin_exits_instead_of_eating_the_marker() {
        assert_eq!(run_to_exit("cat; echo after").await, (vec!["after".to_string()], 0));
    }

    #[tokio::test]
    async fn test_output_that_looks_like_a_marker_does_not_end_the_command() {
        let (stdout, code) = run_to_exit(
            "echo MARKER:0; echo __smelt_done__:nonce:0; sleep 0.2; f() { return 4; }; f",
        )
        .await;
        assert_eq!(stdout, vec!["MARKER:0".to_string(), "__smelt_done__:nonce:0".to_string()]);
        assert_eq!(code, 4);
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

    /// SME-122 review 1, without a socket's timing: an ended shell's last
    /// events, still queued when the terminal's next command starts its new
    /// shell, belong to the command that ended it.
    #[tokio::test]
    async fn test_an_ended_shells_queued_events_name_its_own_command() {
        let state = new_state();
        assert_eq!(super::create_terminal(&state, "t1".to_string()).await, Reply::TerminalCreated);
        let first = state.terminals.lock().await.get("t1").cloned().expect("the terminal");
        start_command(&state, "t1", "c1".to_string(), "echo bye; exit 3").await;
        let deadline = tokio::time::Instant::now() + WAIT;
        while !first.ended.load(Ordering::SeqCst) {
            assert!(tokio::time::Instant::now() < deadline, "the shell didn't end");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Let the ended shell's last events reach the queue.
        tokio::time::sleep(Duration::from_millis(200)).await;
        start_command(&state, "t1", "c2".to_string(), "sleep 0.2; echo hi").await;

        let mut messages = Vec::new();
        while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(2), recv_shell_event(&state)).await {
            messages.extend(event_message(event));
        }
        let exits: Vec<(String, i32)> = messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::Exit { id, code, .. } => Some((id.clone(), *code)),
                _ => None,
            })
            .collect();
        assert_eq!(exits, vec![("c1".to_string(), 3), ("c2".to_string(), 0)], "{messages:?}");
    }

    /// SME-122 review 2: a command claimed just as an idle shell dies (it
    /// holds `current` while the shell's end is being handled) still gets
    /// an exit, instead of being lost and staying running for good.
    #[tokio::test]
    async fn test_a_command_claimed_as_the_shell_dies_still_exits() {
        let state = new_state();
        assert_eq!(super::create_terminal(&state, "t1".to_string()).await, Reply::TerminalCreated);
        let shell = state.terminals.lock().await.get("t1").cloned().expect("the terminal");

        // As `start_command` does, between its `ended` check and its claim.
        let mut current = shell.current.lock().await;
        signal::kill(Pid::from_raw(-(shell.bash_pid as i32)), Signal::SIGKILL).expect("kill the shell");
        tokio::time::sleep(Duration::from_millis(500)).await;
        *current = Some("cB".to_string());
        *shell.marker.lock().unwrap_or_else(|e| e.into_inner()) = Some(new_marker());
        drop(current);

        let mut exits = Vec::new();
        while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(2), recv_shell_event(&state)).await {
            if let Some(AgentMessage::Exit { id, .. }) = event_message(event) {
                exits.push(id);
            }
        }
        assert_eq!(exits, vec!["cB".to_string()]);
    }

    /// An event the agent took off the queue but couldn't send (smelt's end
    /// died before the agent noticed) goes to the next connection, after its
    /// hello, instead of being lost with the socket.
    #[tokio::test]
    async fn test_an_event_that_failed_to_send_goes_to_the_next_connection() {
        let state = new_state();
        let mut dead = Box::pin(futures_util::sink::unfold((), |(), _: Message| async {
            Err::<(), _>(std::io::Error::other("connection reset"))
        }));
        let exit = AgentMessage::Exit { id: "c1".into(), terminal_id: "t1".into(), code: 0 };
        assert!(!send_or_keep(&mut dead, &state, exit.clone()).await, "a failed send reported success");

        let addr = serve_agent_with(state).await;
        let mut client = connect_ready(addr).await;
        assert_eq!(next(&mut client).await, exit);
    }

    /// An event a replaced connection fails to send after the new one has
    /// already opened goes to the new one, not only to the one after it.
    #[tokio::test]
    async fn test_an_event_that_failed_to_send_reaches_the_live_connection() {
        let state = new_state();
        let addr = serve_agent_with(state.clone()).await;
        let mut live = connect_ready(addr).await;

        let mut dead = Box::pin(futures_util::sink::unfold((), |(), _: Message| async {
            Err::<(), _>(std::io::Error::other("connection reset"))
        }));
        let exit = AgentMessage::Exit { id: "c1".into(), terminal_id: "t1".into(), code: 0 };
        assert!(!send_or_keep(&mut dead, &state, exit.clone()).await);
        assert_eq!(next(&mut live).await, exit);
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
