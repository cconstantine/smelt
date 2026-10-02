//! Connections into a pod's ports: which ports may be reached, port-
//! forwards, and listen probes.

use super::*;

/// Refuses the ports in a pod no route may reach: the sandbox agent's
/// (`AGENT_PORT`), whose WebSocket runs commands with no login of its own,
/// and its relay (`RELAY_PORT`), which reaches every container. A preview
/// or a page in the model's browser reaching either would let any website
/// into the sandbox (found in SME-42's code review).
pub fn check_reachable_port(port: u16) -> Result<(), TerminalError> {
    if port == AGENT_PORT || port == RELAY_PORT {
        return Err(TerminalError::AgentPort(port));
    }
    Ok(())
}

/// Where in a conversation's pod a connection goes (SME-33): its own
/// `localhost`, or a Docker container's address in the pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PodHost {
    Localhost,
    Container(std::net::Ipv4Addr),
}

/// A byte stream to a port inside a sandbox pod, from `open_pod_target`.
pub trait PodIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> PodIo for T {}

/// Opens a TCP connection to `host:port` inside `conversation_id`'s own live
/// pod, through a Kubernetes port-forward — the pod is resolved here from
/// the conversation, never named by the caller, so nothing can reach
/// another conversation's pod (SME-42). The connection is made inside the
/// pod's network namespace, so a server bound only to `127.0.0.1` there
/// is reachable. One port-forward per connection.
///
/// Nothing listening on `port` isn't an error here: the stream simply
/// ends without a byte. After the pod side closes, the end of the stream
/// arrives about a second late (seen on k3s), so a caller must never wait
/// for it to know a response is complete.
///
/// `host` is the pod's own `localhost`, or a Docker container's address
/// there (SME-33), reached through the agent's relay.
pub async fn open_pod_target(
    pool: &PgPool,
    conversation_id: i64,
    host: PodHost,
    port: u16,
) -> Result<Box<dyn PodIo>, TerminalError> {
    open_pod_port_with(pool, conversation_id, host, port, || Ok(get()?.client.clone())).await
}

/// `open_pod_target` with the Kubernetes client supplied — a test's own, so
/// it never has to set the process-global manager (see
/// `test_open_pod_port_reaches_the_conversations_own_pod`). The client is
/// only asked for once there's a pod, so a conversation without one never
/// needs a cluster at all.
pub(super) async fn open_pod_port_with(
    pool: &PgPool,
    conversation_id: i64,
    host: PodHost,
    port: u16,
    client: impl FnOnce() -> Result<kube::Client, SandboxError>,
) -> Result<Box<dyn PodIo>, TerminalError> {
    if host == PodHost::Localhost {
        check_reachable_port(port)?;
    }
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    dial_pod(&client()?, &pod_name(pod_id), host, port).await
}

/// Opens a connection to `host:port` in the pod named `pod`: a
/// port-forward for `localhost`, or one to the agent's relay for a
/// container address.
pub(super) async fn dial_pod(
    client: &kube::Client,
    pod: &str,
    host: PodHost,
    port: u16,
) -> Result<Box<dyn PodIo>, TerminalError> {
    let forward_to = match host {
        PodHost::Localhost => {
            check_reachable_port(port)?;
            port
        }
        PodHost::Container(_) => RELAY_PORT,
    };
    let mut forwarder = pods_api(client)
        .portforward(pod, &[forward_to])
        .await
        .map_err(|e| TerminalError::Sandbox(SandboxError::Kube(e)))?;
    let mut stream = forwarder
        .take_stream(forward_to)
        .expect("stream requested for the forwarded port");
    if let PodHost::Container(ip) = host {
        // The relay's one line, then its answer once it has connected
        // (`RELAY_CONNECTED` in src/bin/sandbox_agent.rs). A target it
        // refuses, or can't reach, gets a stream that's already ended,
        // which reads the same as a port nothing listens on.
        use tokio::io::AsyncWriteExt;
        stream
            .write_all(format!("{ip}:{port}\n").as_bytes())
            .await
            .map_err(|e| TerminalError::Sandbox(SandboxError::Io(e)))?;
        let mut answer = [0u8; 3];
        let connected = tokio::time::timeout(RELAY_CONNECT_WAIT, stream.read_exact(&mut answer))
            .await
            .is_ok_and(|read| read.is_ok() && &answer == b"ok\n");
        if !connected {
            return Ok(Box::new(tokio::io::join(tokio::io::empty(), tokio::io::sink())));
        }
    }
    Ok(Box::new(stream))
}

/// Whether something is listening on `port` in `conversation_id`'s pod —
/// the sharing a preview's check, so the model hears at once if it gave
/// the wrong port or the server isn't up yet. A port-forward to a port
/// nothing listens on ends straight away without a byte (about 50ms on
/// k3s); a listening server keeps the connection open waiting for a
/// request. `LISTEN_PROBE` is well past the first and far below a request
/// timeout.
pub async fn pod_port_is_listening(
    pool: &PgPool,
    conversation_id: i64,
    host: PodHost,
    port: u16,
) -> Result<bool, TerminalError> {
    pod_port_is_listening_with(pool, conversation_id, host, port, || Ok(get()?.client.clone())).await
}

pub(super) const LISTEN_PROBE: Duration = Duration::from_millis(500);

/// How long opening the port-forward for the probe may take (SME-51 B8).
pub(super) const LISTEN_OPEN_TIMEOUT: Duration = if cfg!(test) { Duration::from_secs(1) } else { Duration::from_secs(10) };

pub(super) async fn pod_port_is_listening_with(
    pool: &PgPool,
    conversation_id: i64,
    host: PodHost,
    port: u16,
    client: impl FnOnce() -> Result<kube::Client, SandboxError>,
) -> Result<bool, TerminalError> {
    // A port-forward that doesn't open in time says as much as one that's
    // refused: nothing is serving there yet (SME-51 B8).
    let Ok(opened) =
        tokio::time::timeout(LISTEN_OPEN_TIMEOUT, open_pod_port_with(pool, conversation_id, host, port, client)).await
    else {
        return Ok(false);
    };
    Ok(stream_is_listening(opened?).await)
}

/// `pod_port_is_listening`'s judgement of a freshly opened stream.
pub(super) async fn stream_is_listening(mut stream: Box<dyn PodIo>) -> bool {
    let mut first = [0u8; 1];
    match tokio::time::timeout(LISTEN_PROBE, stream.read(&mut first)).await {
        // Still open: something accepted the connection and is waiting.
        Err(_) => true,
        // A server that speaks first (SSH, a database) is listening too.
        Ok(Ok(n)) => n > 0,
        Ok(Err(_)) => false,
    }
}
