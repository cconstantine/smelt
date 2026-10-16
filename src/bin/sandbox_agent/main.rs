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
#[path = "../../docker_net.rs"]
#[allow(dead_code)]
mod docker_net;
// The wire protocol, shared with the server (SME-53).
#[path = "../../agent_protocol.rs"]
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

#[cfg(test)]
mod tests;

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
mod socket_tests;

mod shell;
mod files;
mod commands;
mod client;
mod relay;

use shell::*;
use files::*;
use commands::*;
use client::*;
use relay::*;
