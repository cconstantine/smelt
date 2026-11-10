//! One pod's agent connection: `TerminalConnection` and its request machinery.

use super::*;

/// One pod's agent connection. The WebSocket itself belongs to two tasks
/// `connect` starts: one writes `outgoing`, the other reads the agent's
/// messages (`handle_agent_message`).
pub(in crate::sandbox) struct TerminalConnection {
    pub(in crate::sandbox) outgoing: mpsc::UnboundedSender<String>,
    /// Requests waiting for their reply, by request id. `None` once the
    /// connection has ended: every waiter was told, and nothing new waits.
    pub(in crate::sandbox) pending: StdMutex<Option<HashMap<u64, tokio::sync::oneshot::Sender<Reply>>>>,
    /// The reader and writer tasks, stopped by `close`.
    pub(in crate::sandbox) tasks: StdMutex<Vec<tokio::task::AbortHandle>>,
    /// Request ids are per connection and never reused, so a late reply
    /// can't answer a newer request.
    pub(in crate::sandbox) next_request_id: std::sync::atomic::AtomicU64,
    /// Resolved once, when the connection is first established (see
    /// `connect`) — lets `handle_agent_message` publish a
    /// `SandboxCommandUpdate` for every output line and completion without
    /// a per-line DB round trip. See
    /// SME-10.
    pub(in crate::sandbox) conversation_id: i64,
    /// The pod this connection was dialed to: a script's MCP call is ruled
    /// as this pod's and this conversation's (SME-359), never as anything
    /// the agent says.
    pub(in crate::sandbox) pod_id: i64,
    /// What the agent said it speaks, in its hello.
    pub(in crate::sandbox) agent_version: ProtocolVersion,
}

impl TerminalConnection {
    /// Sends a message that gets no reply (`command`, `signal`).
    pub(in crate::sandbox) fn send(&self, message: &ClientMessage) -> Result<(), AgentRequestError> {
        let text = serde_json::to_string(message).map_err(|e| AgentRequestError::Unencodable(e.to_string()))?;
        self.outgoing.send(text).map_err(|_| AgentRequestError::Disconnected)
    }

    /// Sends the request `build` makes with a fresh request id, and waits
    /// for its reply. The agent's own refusal (`Reply::Error`) comes back as
    /// `Rejected`.
    pub(in crate::sandbox) async fn request(&self, build: impl FnOnce(u64) -> ClientMessage) -> Result<Reply, AgentRequestError> {
        self.request_within(ACK_TIMEOUT, build).await
    }

    /// In an `agent request` span (SME-137) naming the request's action and
    /// timeout, marked failed when it fails. Never the request's content.
    pub(in crate::sandbox) async fn request_within(
        &self,
        timeout: Duration,
        build: impl FnOnce(u64) -> ClientMessage,
    ) -> Result<Reply, AgentRequestError> {
        let request_id = self.next_request_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let message = build(request_id);
        let span = tracing::info_span!(
            "agent request",
            conversation_id = self.conversation_id,
            smelt.agent.action = client_message_action(&message),
            timeout_s = timeout.as_secs() as i64,
            otel.status_code = tracing::field::Empty,
            otel.status_description = tracing::field::Empty,
        );
        crate::telemetry::in_span(span.clone(), async move {
            let result = self.request_message_within(timeout, request_id, message).await;
            if let Err(e) = &result {
                crate::telemetry::mark_error(&span, &e.to_string());
            }
            result
        })
        .await
    }

    async fn request_message_within(
        &self,
        timeout: Duration,
        request_id: u64,
        message: ClientMessage,
    ) -> Result<Reply, AgentRequestError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        match self.pending.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            Some(pending) => pending.insert(request_id, tx),
            None => return Err(AgentRequestError::Disconnected),
        };
        if let Err(e) = self.send(&message) {
            self.forget(request_id);
            return Err(e);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Reply::Error { message })) => Err(AgentRequestError::Rejected(message)),
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err(AgentRequestError::Disconnected),
            Err(_) => {
                self.forget(request_id);
                Err(AgentRequestError::Timeout)
            }
        }
    }

    pub(in crate::sandbox) fn forget(&self, request_id: u64) {
        if let Some(pending) = self.pending.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            pending.remove(&request_id);
        }
    }

    /// Hands `reply` to the request waiting for it. A reply nobody waits for
    /// (its request timed out) is dropped.
    pub(in crate::sandbox) fn resolve(&self, request_id: u64, reply: Reply) {
        let waiter = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .and_then(|pending| pending.remove(&request_id));
        match waiter {
            Some(waiter) => {
                let _ = waiter.send(reply);
            }
            None => tracing::debug!(request_id, "a reply for a request nobody waits for any more"),
        }
    }

    /// Ends every waiting request with `Disconnected` at once, and refuses
    /// new ones: the socket is gone, so no reply is coming.
    pub(in crate::sandbox) fn fail_pending(&self) {
        // Dropping the senders wakes their receivers.
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    /// Fails every waiting request and stops both tasks, which closes the
    /// socket.
    pub(in crate::sandbox) fn close(&self) {
        self.fail_pending();
        for task in self.tasks.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
            task.abort();
        }
    }

    /// Whether the agent has what minor `minor` of this major added. A
    /// feature added in a later minor checks this first, so an older pod
    /// fails that one tool and nothing else.
    #[cfg_attr(not(test), allow(dead_code))] // until a feature needs a minor above 0
    pub(in crate::sandbox) fn require_minor(&self, minor: u32) -> Result<(), TerminalError> {
        if self.agent_version.minor >= minor {
            Ok(())
        } else {
            Err(TerminalError::AgentTooOld {
                needs: ProtocolVersion { major: PROTOCOL_VERSION.major, minor },
                found: self.agent_version,
            })
        }
    }
}

/// Opens a WebSocket to `pod_id`'s agent over `dialer` and starts the two
/// tasks that own it: one drains `outgoing` into the socket, the other
/// hands every agent message to `handle_agent_message` (output and exits
/// into `terminal_events`/`terminal_commands`, replies to their waiting
/// request). When the socket ends unexpectedly, the reader reconnects, or
/// confirms a crash. One connection per **pod**, shared by every terminal
/// it hosts — see SME-9's "Why N pods and N terminals."
/// A request's `action`, as the agent's protocol names it, for its span.
fn client_message_action(message: &ClientMessage) -> &'static str {
    match message {
        ClientMessage::CreateTerminal { .. } => "create_terminal",
        ClientMessage::TerminateTerminal { .. } => "terminate_terminal",
        ClientMessage::Command { .. } => "command",
        ClientMessage::Signal { .. } => "signal",
        ClientMessage::ReadFile { .. } => "read_file",
        ClientMessage::WriteFile { .. } => "write_file",
        ClientMessage::EditFile { .. } => "edit_file",
        ClientMessage::ListDirectory { .. } => "list_directory",
        ClientMessage::Glob { .. } => "glob",
        ClientMessage::Grep { .. } => "grep",
        ClientMessage::McpResponse { .. } => "mcp_response",
    }
}
