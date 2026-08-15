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
    }
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
