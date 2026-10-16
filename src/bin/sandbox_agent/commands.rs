use super::*;

/// One line of output, or the completion marker — what a terminal's reader
/// task (below) turns its shell's raw stdout/stderr into, tagged with
/// which terminal it came from since every terminal's reader now feeds one
/// shared channel. `seq` resets to 0 every time a `Marker` is emitted,
/// since it's scoped to the command that just finished, not the shell's
/// whole lifetime — matches the WS protocol's per-command `seq` exactly.
/// Each names its command (`id`, from the shell it came from), not left to
/// be looked up by terminal when it's sent: by then the terminal may have
/// a new shell, running another command (review 1).
pub(super) enum ShellEvent {
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
pub(super) async fn run_reader(
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
pub(super) const MAX_OUTPUT_CHUNK: usize = 64 * 1024;

/// How much of a long line's end `take_piece` holds back for the next
/// piece: more than a marker and its exit code (`new_marker`: 40 bytes,
/// plus up to 11), so one is never split across two pieces.
const MARKER_TAIL: usize = 64;

/// How long after a command's stdout marker its stderr marker may take
/// before the command is finished without it (B12, SME-122).
pub(super) const STDERR_MARKER_WAIT: Duration = Duration::from_secs(1);

/// Where the in-flight command's end has got to: see `run_reader`. Each of
/// its two markers (`o`, written to the shell's stdout, `e` to its stderr)
/// carries the exit code and says which it is, whichever stream it arrives
/// on: a command can send the shell's stdout or stderr elsewhere, or swap
/// them (review 2).
#[derive(Default)]
pub(super) struct Ending {
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
    pub(super) fn find(&self, marker: &std::sync::Mutex<Option<String>>, line: &str) -> Option<(String, EndMarker)> {
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
    pub(super) fn note(&mut self, meaning: &EndMarker) {
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
    pub(super) fn give_up_waiting(&mut self) {
        self.out_seen = true;
        self.err_seen = true;
    }

    /// The exit code, once both markers have come: clears the in-flight
    /// marker and starts over for the next command.
    pub(super) fn complete(&mut self, marker: &std::sync::Mutex<Option<String>>) -> Option<i32> {
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
pub(super) enum EndMarker {
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
pub(super) struct Shell {
    pub(super) stdin: AsyncMutex<ChildStdin>,
    /// Also this shell's own process group id: spawned with
    /// `.process_group(0)`, so `bash_pid` and the shell's pgid are the same
    /// number — see `terminate_terminal` and SME-9's "Per-terminal
    /// process groups."
    pub(super) bash_pid: u32,
    /// The one in-flight command's id in *this* terminal, if any — the
    /// single source of truth the single-command-in-flight-per-terminal
    /// design relies on. The *primary* enforcement of "only one at a time"
    /// is server-side (before a command is ever sent here at all — see
    /// SME-9's `sandbox.rs` bullet); this is the agent's own defensive
    /// backstop.
    pub(super) current: AsyncMutex<Option<String>>,
    /// The in-flight command's completion marker (`new_marker`), shared
    /// with this terminal's `run_reader`, which clears it on a match.
    pub(super) marker: Arc<std::sync::Mutex<Option<String>>>,
    /// The shell's process, waited on once it has ended (`shell_exit_code`).
    /// Not killed on drop (tokio's default) — `terminate_terminal`'s
    /// explicit `killpg` is what actually ends it.
    pub(super) bash_child: AsyncMutex<Child>,
    /// Set once the shell has ended (`finish_ended_shell`): the terminal's
    /// next command starts a new one.
    pub(super) ended: std::sync::atomic::AtomicBool,
}
