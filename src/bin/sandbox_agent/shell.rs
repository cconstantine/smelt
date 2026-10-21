use super::*;

/// Finds a command's completion marker (`marker`, which ends in that
/// command's nonce and a `:`) at the end of a line, and returns the output
/// in front of it (non-empty when the command's last output had no
/// newline), which of the two markers it is (`o`, written to stdout, or
/// `e`, to stderr; review 2) and the exit code after it.
pub(super) fn split_marker<'a>(line: &'a str, marker: &str) -> Option<(&'a str, char, i32)> {
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
pub(super) fn new_marker() -> String {
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

/// Spawns a new named shell — `.process_group(0)` gives it a process group
/// distinct from the agent's and from every sibling terminal's (see
/// SME-9's "Per-terminal process groups"), so `terminate_terminal` can
/// `killpg` it in isolation later. `set -m`/`trap ':' INT` are the same
/// per-shell startup this design has always used, just no longer only at
/// agent-launch time.
pub(super) async fn create_terminal(state: &Arc<AppState>, terminal_id: String) -> Reply {
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
        ended: std::sync::atomic::AtomicBool::new(false),
    });
    // The reader owns the shell's process (it alone waits on it), which
    // isn't killed on drop (tokio's default): `terminate_terminal`'s
    // explicit `killpg` is what ends it.
    tokio::spawn(run_reader(
        terminal_id.to_string(),
        stdout,
        stderr,
        child,
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
pub(super) async fn terminate_terminal(state: &Arc<AppState>, terminal_id: String) -> Reply {
    let shell = state.terminals.lock().await.remove(&terminal_id);
    let Some(shell) = shell else {
        return reply_error("unknown terminal_id");
    };
    // An ended shell has been reaped (by its reader), so its group id is
    // free and could be another process's (SME-191). Jobs it left running
    // live on, as they do when a running shell's group is killed.
    if shell.ended.load(Ordering::SeqCst) {
        return Reply::TerminalTerminated;
    }

    if let Err(err) = signal::kill(Pid::from_raw(-(shell.bash_pid as i32)), Signal::SIGKILL) {
        tracing::warn!(%err, %terminal_id, "killpg failed while terminating terminal");
    }

    Reply::TerminalTerminated
}

pub(super) async fn start_command(state: &Arc<AppState>, terminal_id: &str, id: String, command: &str) {
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
    // The shell ended between `shell_for_command` and this lock: it has
    // been reaped or its end handled (both under this lock), so a command
    // claimed now would be written to a dead shell. Go round again for a
    // new shell (review 2; SME-191 review 1).
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

pub(super) async fn signal_current_command(state: &Arc<AppState>, terminal_id: &str, id: &str, signal: &str) {
    let Some(shell) = state.terminals.lock().await.get(terminal_id).cloned() else {
        tracing::info!(%terminal_id, "send_signal: unknown terminal_id, ignoring");
        return;
    };

    let current = shell.current.lock().await;
    // A reaped shell's pid is free, so its `/proc` children could be
    // another process's (SME-191 review 1).
    if shell.ended.load(Ordering::SeqCst) {
        tracing::info!(%terminal_id, %id, "send_signal: the shell has ended, ignoring");
        return;
    }
    let matches_current = current.as_deref() == Some(id);
    drop(current);
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
