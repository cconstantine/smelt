use super::*;

pub(super) struct AppState {
    pub(super) terminals: AsyncMutex<HashMap<String, Arc<Shell>>>,
    /// Kept alive here (in addition to being cloned into every terminal's
    /// reader task) so the channel never closes just because the map of
    /// terminals happens to be momentarily empty — a pod legitimately has
    /// zero terminals between `terminate_terminal` and the next
    /// `create_terminal`, and the connection must survive that.
    pub(super) events_tx: mpsc::UnboundedSender<ShellEvent>,
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

pub(super) fn new_state() -> Arc<AppState> {
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

pub(super) fn router(state: Arc<AppState>) -> Router {
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
pub(super) fn event_message(event: ShellEvent) -> Option<AgentMessage> {
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

pub(super) async fn recv_shell_event(state: &Arc<AppState>) -> Option<ShellEvent> {
    state.events_rx.lock().await.recv().await
}

/// Sends an event to smelt. If the send fails, the event is kept for the
/// next connection (`AppState::unsent`) and this returns false: the event
/// was already taken off `events_rx`, and an exit lost here would leave its
/// command running in smelt's records for good.
pub(super) async fn send_or_keep<S>(sink: &mut S, state: &AppState, message: AgentMessage) -> bool
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
