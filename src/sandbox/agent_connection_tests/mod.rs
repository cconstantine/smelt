use super::*;
use crate::agent_protocol::{DirEntry, Reply};
use std::sync::atomic::{AtomicUsize, Ordering};
use crate::api::pods::AgentStatus;

/// What the fake agent sends first on every connection.
#[derive(Clone)]
enum Greeting {
    Hello(ProtocolVersion),
    /// A raw text frame, e.g. a protocol-0 output line.
    Raw(String),
    Nothing,
}

enum Out {
    Message(AgentMessage),
    Raw(String),
    Close,
}

/// One connection the fake agent accepted: what smelt sent on it, and a
/// way to answer.
struct FakeConnection {
    received: mpsc::UnboundedReceiver<ClientMessage>,
    send: mpsc::UnboundedSender<Out>,
    /// Fires when the socket ends, whoever ended it.
    ended: tokio::sync::oneshot::Receiver<()>,
}

impl FakeConnection {
    async fn next_request(&mut self) -> ClientMessage {
        tokio::time::timeout(Duration::from_secs(5), self.received.recv())
            .await
            .expect("smelt sent nothing within 5s")
            .expect("the connection ended")
    }

    fn reply(&self, request_id: u64, result: Reply) {
        let _ = self.send.send(Out::Message(AgentMessage::Reply { request_id, result }));
    }
}

struct FakeAgent {
    connections: mpsc::UnboundedReceiver<FakeConnection>,
    dials: Arc<AtomicUsize>,
}

impl FakeAgent {
    async fn next_connection(&mut self) -> FakeConnection {
        tokio::time::timeout(Duration::from_secs(5), self.connections.recv())
            .await
            .expect("smelt didn't connect within 5s")
            .expect("the fake agent stopped")
    }
}

/// Dials the fake agent's port, after `delay`, counting dials.
struct FakeDialer {
    addr: std::net::SocketAddr,
    delay: Duration,
    dials: Arc<AtomicUsize>,
}

impl AgentDialer for FakeDialer {
    fn dial(&self, _pod_id: i64) -> BoxFuture<'_, Result<Box<dyn AgentIo>, SandboxError>> {
        Box::pin(async move {
            self.dials.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            let stream = tokio::net::TcpStream::connect(self.addr).await.map_err(SandboxError::Io)?;
            Ok(Box::new(stream) as Box<dyn AgentIo>)
        })
    }

    fn is_running(&self, _pod_id: i64) -> BoxFuture<'_, Result<bool, SandboxError>> {
        Box::pin(async { Ok(true) })
    }

    fn death_reason(&self, _pod_id: i64) -> BoxFuture<'_, Option<Option<String>>> {
        Box::pin(async { None })
    }

    fn image(&self, pod_id: i64) -> BoxFuture<'_, Option<String>> {
        let held = image_reads_held().lock().unwrap_or_else(|e| e.into_inner()).get(&pod_id).cloned();
        Box::pin(async move {
            if let Some((reading, release)) = held {
                reading.notify_one();
                release.notified().await;
            }
            fake_images().lock().unwrap_or_else(|e| e.into_inner()).get(&pod_id).cloned()
        })
    }
}

/// Pods whose image read waits: it signals the first `Notify` once it has
/// started, then waits for the second.
#[allow(clippy::type_complexity)]
fn image_reads_held() -> &'static StdMutex<HashMap<i64, (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>> {
    static HELD: LazyLock<StdMutex<HashMap<i64, (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>> =
        LazyLock::new(Default::default);
    &HELD
}

/// The image each fake pod "runs", by pod id; a pod with none reads as
/// unknown.
fn fake_images() -> &'static StdMutex<HashMap<i64, String>> {
    static IMAGES: LazyLock<StdMutex<HashMap<i64, String>>> = LazyLock::new(Default::default);
    &IMAGES
}

/// A fake agent for `pod_id`, reached through `dialer_for`.
async fn fake_agent(pod_id: i64, greeting: Greeting, dial_delay: Duration) -> FakeAgent {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (connections_tx, connections) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else { continue };
            let (received_tx, received) = mpsc::unbounded_channel();
            let (send, mut outgoing) = mpsc::unbounded_channel::<Out>();
            let (ended_tx, ended) = tokio::sync::oneshot::channel();
            let _ = connections_tx.send(FakeConnection { received, send, ended });
            let greeting = greeting.clone();
            tokio::spawn(async move {
                let first = match greeting {
                    Greeting::Hello(version) => Some(serde_json::to_string(&AgentMessage::Hello(version)).expect("json")),
                    Greeting::Raw(text) => Some(text),
                    Greeting::Nothing => None,
                };
                if let Some(first) = first {
                    let _ = ws.send(WsMessage::Text(first.into())).await;
                }
                loop {
                    tokio::select! {
                        frame = ws.next() => match frame {
                            Some(Ok(WsMessage::Text(text))) => {
                                let message = serde_json::from_str(&text).expect("smelt sent a v1 message");
                                let _ = received_tx.send(message);
                            }
                            Some(Ok(_)) => {}
                            _ => break,
                        },
                        out = outgoing.recv() => match out {
                            Some(Out::Message(message)) => {
                                let text = serde_json::to_string(&message).expect("json");
                                let _ = ws.send(WsMessage::Text(text.into())).await;
                            }
                            Some(Out::Raw(text)) => {
                                let _ = ws.send(WsMessage::Text(text.into())).await;
                            }
                            Some(Out::Close) | None => {
                                let _ = ws.close(None).await;
                                break;
                            }
                        },
                    }
                }
                let _ = ended_tx.send(());
            });
        }
    });
    let dials = Arc::new(AtomicUsize::new(0));
    let dialer = FakeDialer { addr, delay: dial_delay, dials: dials.clone() };
    test_dialers().lock().unwrap_or_else(|e| e.into_inner()).insert(pod_id, Arc::new(dialer));
    FakeAgent { connections, dials }
}

/// A conversation and a live pod row for it, with ids no other test uses:
/// the registry and the fake dialers are keyed by pod id, and the start
/// lock a reconnect checks (`start_under_way_for_pod`) by conversation id.
/// With the conversation numbered from 1 in the test's own database,
/// another test starting a pod for its conversation 1 made a reconnect
/// here see a start under way and answer `NoPod` (SME-341: CI run
/// 41366729038, "connects: NoPod").
async fn pod_row(pool: &PgPool) -> (i64, i64) {
    static NEXT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let unique = (std::process::id() as i64 % 10_000) * 1_000_000
        + NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        + 5_000_000_000;
    sqlx::query("SELECT setval(pg_get_serial_sequence('sandbox_pods', 'id'), $1)")
        .bind(unique)
        .execute(pool)
        .await
        .expect("move the pod id sequence");
    sqlx::query("SELECT setval(pg_get_serial_sequence('conversations', 'id'), $1)")
        .bind(unique)
        .execute(pool)
        .await
        .expect("move the conversation id sequence");
    let conversation = db::create_conversation(pool).await.expect("conversation");
    let pod = db::create_sandbox_pod(pool, conversation.id).await.expect("pod row");
    (conversation.id, pod.id)
}

fn current() -> Greeting {
    Greeting::Hello(PROTOCOL_VERSION)
}

mod requests;
mod versions;
mod lifecycle;
