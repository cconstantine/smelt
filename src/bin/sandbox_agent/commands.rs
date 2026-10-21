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
/// It owns the shell's process and is the only code that waits on it.
/// The shell's exit ends the reading too, once its own output has been
/// read, not only its pipes' end: a background job the shell started
/// keeps them open (SME-191).
pub(super) async fn run_reader(
    terminal_id: String,
    mut stdout: BufReader<ChildStdout>,
    mut stderr: BufReader<ChildStderr>,
    mut child: Child,
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
    // The shell's status once it has exited (SME-191). Its own last words
    // are all in its pipes by the time it's reaped, so each pipe is read
    // as far as what was in it then, however long that takes (review 1: a
    // fixed time for it lost output on a busy node). A pipe that hasn't
    // reached its end there is held by a job the shell left running; with
    // no job, it always has.
    let mut exit: Option<i32> = None;
    let (mut stdout_read, mut stderr_read) = (ReadSoFar::default(), ReadSoFar::default());
    let (mut stdout_held, mut stderr_held) = (false, false);
    while stdout_open || stderr_open {
        tokio::select! {
            result = fill_piece(&mut stdout, &mut stdout_buf, &mut stdout_read), if stdout_open => {
                match result {
                    // The pipe's end or an error, or, once the shell has
                    // been reaped, the end of its own output.
                    Ok(false) | Err(_) => {
                        stdout_open = false;
                        stdout_held = exit.is_some() && !writers_gone(stdout.get_ref());
                    }
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
            result = fill_piece(&mut stderr, &mut stderr_buf, &mut stderr_read), if stderr_open => {
                match result {
                    Ok(false) | Err(_) => {
                        stderr_open = false;
                        stderr_held = exit.is_some() && !writers_gone(stderr.get_ref());
                    }
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
            // Cancel safe, and fused once it has returned (tokio 1.53.1).
            status = child.wait(), if exit.is_none() => {
                exit = Some(exit_code_of(status));
                // Ended as soon as it's reaped (review 1): its pid is free
                // now, and a command claimed from here on, before the end
                // is finished below, must go to a new shell. Under
                // `current`'s lock, which `start_command` claims under.
                let current = shell.current.lock().await;
                shell.ended.store(true, Ordering::SeqCst);
                drop(current);
                stdout_read.end_at(stdout.buffer().len(), unread_in_pipe(stdout.get_ref()));
                stderr_read.end_at(stderr.buffer().len(), unread_in_pipe(stderr.get_ref()));
                #[cfg(test)]
                test_hooks::after_reap(&terminal_id).await;
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
    let jobs_left = stdout_held || stderr_held;
    let code = match exit {
        Some(code) => code,
        // Both pipes closed and the shell hasn't exited (yet).
        None => wait_for_closed_shell(&mut child, shell.bash_pid).await,
    };
    // Ended between the two markers: the command did finish.
    if let Some(exit_code) = ending.code.take() {
        marker.lock().unwrap_or_else(|e| e.into_inner()).take();
        let id = shell.current.lock().await.take();
        if tx.send(ShellEvent::Marker { terminal_id: terminal_id.clone(), id, exit_code }).is_err() {
            return;
        }
        seq = 0;
    }
    finish_ended_shell(&terminal_id, &shell, seq, code, jobs_left, &tx).await;
    if jobs_left {
        drop(shell);
        discard_until_closed(stdout, stderr, stdout_held, stderr_held).await;
    }
}

/// Reads an ended shell's pipes to their end and throws the output away
/// (SME-191): a job the shell left running holds them, and it must
/// neither block on a full pipe (if they weren't read) nor die of SIGPIPE
/// (if they were closed). Its output reaches no command.
async fn discard_until_closed(
    mut stdout: BufReader<ChildStdout>,
    mut stderr: BufReader<ChildStderr>,
    mut stdout_open: bool,
    mut stderr_open: bool,
) {
    let (mut out_buf, mut err_buf) = (vec![0u8; 8192], vec![0u8; 8192]);
    while stdout_open || stderr_open {
        tokio::select! {
            read = stdout.read(&mut out_buf), if stdout_open => {
                stdout_open = matches!(read, Ok(n) if n > 0);
            }
            read = stderr.read(&mut err_buf), if stderr_open => {
                stderr_open = matches!(read, Ok(n) if n > 0);
            }
        }
    }
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

/// How far one of a shell's pipes has been read (SME-191): `pulled` bytes
/// in all, and, once the shell has been reaped, `until`, the end of the
/// shell's own output in it. What comes after that is a job's.
#[derive(Default)]
struct ReadSoFar {
    pulled: u64,
    until: Option<u64>,
}

impl ReadSoFar {
    /// At the reap: the shell's output ends after what the reader has
    /// buffered (`buffered`) and what's still in the pipe (`in_pipe`).
    /// When the pipe couldn't be asked, it's read to its end, as before
    /// SME-191.
    fn end_at(&mut self, buffered: usize, in_pipe: Option<usize>) {
        match in_pipe {
            Some(in_pipe) => self.until = Some(self.pulled + buffered as u64 + in_pipe as u64),
            None => tracing::warn!("couldn't read how much of an ended shell's output is left; reading to the pipe's end"),
        }
    }

    /// How many more bytes may be read: `None` for no limit.
    fn left(&self) -> Option<usize> {
        self.until.map(|until| usize::try_from(until.saturating_sub(self.pulled)).unwrap_or(usize::MAX))
    }
}

/// How many bytes are waiting in a pipe (`FIONREAD`), or `None` if it
/// can't be asked.
fn unread_in_pipe(pipe: &impl std::os::fd::AsRawFd) -> Option<usize> {
    let mut unread: nix::libc::c_int = 0;
    // SAFETY: `FIONREAD` writes one `c_int`, to `unread`.
    let result = unsafe { nix::libc::ioctl(pipe.as_raw_fd(), nix::libc::FIONREAD, &mut unread) };
    if result != 0 {
        return None;
    }
    usize::try_from(unread).ok()
}

/// Whether nothing holds a pipe's write end any more: `poll` says it has
/// hung up (data may still be waiting in it).
fn writers_gone(pipe: &impl std::os::fd::AsRawFd) -> bool {
    let mut fd = nix::libc::pollfd { fd: pipe.as_raw_fd(), events: nix::libc::POLLIN, revents: 0 };
    // SAFETY: one valid `pollfd`, and a zero timeout.
    let ready = unsafe { nix::libc::poll(&mut fd, 1, 0) };
    ready == 1 && fd.revents & nix::libc::POLLHUP != 0
}

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
/// `jobs_left`: a job the shell started still holds its output (SME-191),
/// which the note says, since the job is left running.
async fn finish_ended_shell(
    terminal_id: &str,
    shell: &Shell,
    seq: u64,
    code: i32,
    jobs_left: bool,
    tx: &mpsc::UnboundedSender<ShellEvent>,
) {
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
            data: if jobs_left {
                format!(
                    "smelt: this terminal's shell exited (status {code}), and background jobs it started are still running; their output is no longer shown (redirect a job's output to a file to keep it), and the new shell's `jobs` won't list them (stop one with `kill <pid>` or `pkill`). Its next command runs in a new shell, with the working directory and variables reset"
                )
            } else {
                format!(
                    "smelt: this terminal's shell exited (status {code}); its next command runs in a new shell, with the working directory and variables reset"
                )
            },
        });
        let _ = tx.send(ShellEvent::Marker { terminal_id: terminal_id.to_string(), id, exit_code: code });
    }
}

/// How long a shell's process has to exit once both its pipes have
/// closed, before its process group is killed.
const SHELL_EXIT_WAIT: Duration = Duration::from_secs(5);

/// The status of a shell whose pipes have both closed: it's waited on for
/// `SHELL_EXIT_WAIT`, and one that hasn't exited by then is killed first.
async fn wait_for_closed_shell(child: &mut Child, bash_pid: u32) -> i32 {
    let status = match tokio::time::timeout(SHELL_EXIT_WAIT, child.wait()).await {
        Ok(status) => status,
        Err(_) => {
            let _ = signal::kill(Pid::from_raw(-(bash_pid as i32)), Signal::SIGKILL);
            child.wait().await
        }
    };
    exit_code_of(status)
}

/// An exit status as a shell reports one: its code, or 128 plus the
/// signal that killed it; 255 when it couldn't be waited on.
fn exit_code_of(status: std::io::Result<std::process::ExitStatus>) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match status {
        Ok(status) => status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
        Err(_) => 255,
    }
}

/// Reads into `buf` until it holds a whole line or `MAX_OUTPUT_CHUNK`
/// bytes (B13, SME-122), or the stream ends. True when there's a piece to
/// take (`take_piece`); false at the end of the stream, or of an ended
/// shell's own output (`read.until`, SME-191), with nothing left.
/// Cancel safe: `fill_buf` is, and what's read is moved into `buf`, kept
/// across calls, and counted in `read`, before the next await.
async fn fill_piece<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    read: &mut ReadSoFar,
) -> std::io::Result<bool> {
    loop {
        if buf.last() == Some(&b'\n') || buf.len() >= MAX_OUTPUT_CHUNK {
            return Ok(true);
        }
        let left = read.left().unwrap_or(usize::MAX);
        if left == 0 {
            // A last line with no newline is a piece of its own, as at
            // the stream's end (review 1).
            return Ok(!buf.is_empty());
        }
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(!buf.is_empty());
        }
        let room = (MAX_OUTPUT_CHUNK - buf.len()).min(available.len()).min(left);
        let take = available[..room].iter().position(|&b| b == b'\n').map_or(room, |at| at + 1);
        buf.extend_from_slice(&available[..take]);
        reader.consume(take);
        read.pulled += take as u64;
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
    /// Set once the shell has ended: as soon as its reader reaps it
    /// (SME-191), or, if its pipes close first, by `finish_ended_shell`.
    /// The terminal's next command starts a new one.
    pub(super) ended: std::sync::atomic::AtomicBool,
}

/// Pauses in `run_reader` for tests (SME-191 review 1), keyed by terminal
/// id so tests running at once don't meet.
#[cfg(test)]
pub(super) mod test_hooks {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::Notify;

    /// A pause at one point in a terminal's reader: `reached` is notified
    /// when the reader gets there, and it waits there for `release` (at
    /// most `HOLD`, so a test that fails first doesn't wedge it for good).
    #[derive(Default)]
    pub(crate) struct Pause {
        pub(crate) reached: Notify,
        pub(crate) release: Notify,
    }

    const HOLD: Duration = Duration::from_secs(30);

    static AFTER_REAP: Mutex<Vec<(String, Arc<Pause>)>> = Mutex::new(Vec::new());

    /// Pauses `terminal_id`'s reader the next time it reaps its shell.
    pub(crate) fn pause_after_reap(terminal_id: &str) -> Arc<Pause> {
        let pause = Arc::new(Pause::default());
        AFTER_REAP
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((terminal_id.to_string(), pause.clone()));
        pause
    }

    pub(crate) async fn after_reap(terminal_id: &str) {
        let pause = {
            let mut pauses = AFTER_REAP.lock().unwrap_or_else(|e| e.into_inner());
            let at = pauses.iter().position(|(id, _)| id == terminal_id);
            at.map(|at| pauses.swap_remove(at).1)
        };
        if let Some(pause) = pause {
            pause.reached.notify_one();
            let _ = tokio::time::timeout(HOLD, pause.release.notified()).await;
        }
    }
}
