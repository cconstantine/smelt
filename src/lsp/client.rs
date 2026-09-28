//! A Language Server Protocol client over any byte stream: JSON-RPC with
//! `Content-Length` framing, as a language server speaks it on stdio.
//!
//! What it does beyond sending and receiving (SME-35's state model):
//! - A request that times out is dropped from the pending list and the
//!   server is sent `$/cancelRequest`; the client stays usable.
//! - A request whose caller goes away (a Stop drops the turn) is simply
//!   forgotten; its response is ignored when it arrives.
//! - The server's own requests (`workspace/configuration`, progress tokens,
//!   capability registration) are answered through the handler.
//! - When the stream ends, every pending request fails with `Closed`, and
//!   so does every later one: the client is broken, and its owner
//!   reconnects.
//!
//! It's smelt's own rather than async-lsp, which pins an older lsp-types,
//! uses the futures I/O traits rather than tokio's, and can't send
//! `$/cancelRequest` for a request that times out. Messages stay JSON
//! values: the operations read a few fields of each answer, and servers
//! differ in which optional shapes they send.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::oneshot;

/// Why a request didn't get a result.
#[derive(Debug, Clone, PartialEq)]
pub enum LspError {
    /// No answer within the time given; the server was asked to cancel.
    Timeout(Duration),
    /// The stream ended: the server process (or its pod) is gone.
    Closed,
    /// The server answered with an error.
    Server { code: i64, message: String },
}

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LspError::Timeout(after) => write!(f, "the language server didn't answer within {}s", after.as_secs()),
            LspError::Closed => write!(f, "the language server's connection closed (it stopped or crashed)"),
            LspError::Server { code, message } => write!(f, "the language server refused ({code}): {message}"),
        }
    }
}

/// A message the server sent that isn't an answer to one of ours.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// A notification: `textDocument/publishDiagnostics`, `$/progress`...
    Notification { method: String, params: Value },
    /// A request, whose result the handler returns.
    Request { method: String, params: Value },
}

/// What the client does with the server's messages: notifications are
/// passed on, and a server request gets back whatever this returns (`null`
/// for most).
pub type Handler = Arc<dyn Fn(Incoming) -> Value + Send + Sync>;

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, LspError>>>>>;

/// One connection to one language server.
pub struct LspClient {
    writer: Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    pending: Pending,
    next_id: AtomicI64,
    closed: Arc<AtomicBool>,
    /// The read loop, stopped with the client: it holds the writer too, so
    /// while it runs the server's stdin stays open and the server with it.
    reading: tokio::task::AbortHandle,
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // With the read loop gone, the last handle on the writer goes with
        // the client, which closes the server's stdin.
        self.reading.abort();
    }
}

/// The bytes of one message: a `Content-Length` header, then the JSON.
pub fn frame(message: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(message).expect("a JSON value always serializes");
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend(body);
    out
}

/// Reads the next message from `reader`, keeping any bytes past it in
/// `buffer` for the next call. `None` at the end of the stream, or if the
/// stream stops being LSP.
pub async fn read_message<R: AsyncRead + Unpin>(reader: &mut R, buffer: &mut Vec<u8>) -> Option<Value> {
    loop {
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let header = std::str::from_utf8(&buffer[..end]).ok()?;
            let length: usize = header
                .split("\r\n")
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.trim().eq_ignore_ascii_case("content-length").then(|| value.trim().parse().ok())?
                })?;
            let start = end + 4;
            if buffer.len() >= start + length {
                let message = serde_json::from_slice(&buffer[start..start + length]).ok();
                buffer.drain(..start + length);
                return message;
            }
        }
        let mut chunk = [0u8; 8192];
        let read = reader.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

impl LspClient {
    /// Starts a client on a server's stdout (`reader`) and stdin
    /// (`writer`), with `handler` for what the server sends unasked.
    pub fn start<R, W>(reader: R, writer: W, handler: Handler) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let writer: Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>> =
            Arc::new(tokio::sync::Mutex::new(Box::new(writer)));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let reading = tokio::spawn(read_loop(reader, writer.clone(), pending.clone(), closed.clone(), handler)).abort_handle();
        LspClient { writer, pending, next_id: AtomicI64::new(1), closed, reading }
    }

    /// Ends the connection: closes the server's stdin (a language server
    /// exits when it ends) and fails every waiting request.
    pub async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.reading.abort();
        let _ = self.writer.lock().await.shutdown().await;
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Whether the connection has ended.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Sends `method` with `params` and waits up to `timeout` for the
    /// result.
    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, LspError> {
        if self.is_closed() {
            return Err(LspError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, sender);
        // Forgotten if this future is dropped (a Stop): the answer, when it
        // comes, finds no one waiting.
        let forget = Forget { pending: &self.pending, id };
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await?;
        let answer = tokio::time::timeout(timeout, receiver).await;
        drop(forget);
        match answer {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(LspError::Closed),
            Err(_) => {
                let _ = self.notify("$/cancelRequest", json!({"id": id})).await;
                Err(LspError::Timeout(timeout))
            }
        }
    }

    /// Sends a notification.
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        if self.is_closed() {
            return Err(LspError::Closed);
        }
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params})).await
    }

    async fn send(&self, message: &Value) -> Result<(), LspError> {
        write_message(&self.writer, message).await.map_err(|_| {
            self.closed.store(true, Ordering::SeqCst);
            LspError::Closed
        })
    }

}

/// Removes a request from the pending list when its caller stops waiting,
/// however that happens.
struct Forget<'a> {
    pending: &'a Pending,
    id: i64,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.id);
    }
}

async fn write_message(
    writer: &tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    message: &Value,
) -> std::io::Result<()> {
    let mut writer = writer.lock().await;
    writer.write_all(&frame(message)).await?;
    writer.flush().await
}

/// Reads the server's messages until the stream ends: answers go to their
/// requests, the rest to `handler`. At the end, every waiting request
/// fails `Closed`.
async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: R,
    writer: Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    pending: Pending,
    closed: Arc<AtomicBool>,
    handler: Handler,
) {
    let mut buffer = Vec::new();
    while let Some(message) = read_message(&mut reader, &mut buffer).await {
        let method = message.get("method").and_then(Value::as_str).map(str::to_string);
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (method, message.get("id").cloned()) {
            // An answer to one of ours.
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else { continue };
                let result = match message.get("error") {
                    Some(error) => Err(LspError::Server {
                        code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                        message: error.get("message").and_then(Value::as_str).unwrap_or_default().to_string(),
                    }),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let waiting = pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                if let Some(waiting) = waiting {
                    let _ = waiting.send(result);
                }
            }
            (Some(method), Some(id)) => {
                let result = handler(Incoming::Request { method, params });
                let answer = json!({"jsonrpc": "2.0", "id": id, "result": result});
                if write_message(&writer, &answer).await.is_err() {
                    break;
                }
            }
            (Some(method), None) => {
                handler(Incoming::Notification { method, params });
            }
            (None, None) => {}
        }
    }
    closed.store(true, Ordering::SeqCst);
    // Dropping the senders fails every waiting request with `Closed`.
    pending.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{DuplexStream, duplex};

    /// A stand-in language server on the other end of an in-memory pipe:
    /// the test reads what the client sent and writes what the server says.
    struct StandIn {
        from_client: DuplexStream,
        to_client: DuplexStream,
        buffer: Vec<u8>,
    }

    impl StandIn {
        async fn next(&mut self) -> Value {
            tokio::time::timeout(Duration::from_secs(5), read_message(&mut self.from_client, &mut self.buffer))
                .await
                .expect("the client should send something")
                .expect("a message")
        }

        async fn send(&mut self, message: Value) {
            self.to_client.write_all(&frame(&message)).await.expect("write");
        }
    }

    fn connect(handler: Handler) -> (LspClient, StandIn) {
        let (client_out, from_client) = duplex(1 << 16);
        let (to_client, client_in) = duplex(1 << 16);
        let client = LspClient::start(client_in, client_out, handler);
        (client, StandIn { from_client, to_client, buffer: Vec::new() })
    }

    fn quiet() -> Handler {
        Arc::new(|_| Value::Null)
    }

    #[tokio::test]
    async fn test_a_message_is_read_across_any_chunk_boundaries() {
        let (mut writer, mut reader) = duplex(1 << 16);
        let first = frame(&json!({"jsonrpc": "2.0", "id": 1, "result": "héllo"}));
        let second = frame(&json!({"jsonrpc": "2.0", "method": "x", "params": {}}));
        let bytes: Vec<u8> = first.iter().chain(second.iter()).copied().collect();
        tokio::spawn(async move {
            // One byte at a time, then the rest: splits the header, the
            // blank line and a multi-byte character.
            for chunk in bytes.chunks(1).take(30) {
                writer.write_all(chunk).await.unwrap();
                tokio::task::yield_now().await;
            }
            writer.write_all(&bytes[30..]).await.unwrap();
        });
        let mut buffer = Vec::new();
        let a = read_message(&mut reader, &mut buffer).await.expect("first");
        let b = read_message(&mut reader, &mut buffer).await.expect("second");
        assert_eq!(a["result"], "héllo");
        assert_eq!(b["method"], "x");
    }

    #[tokio::test]
    async fn test_answers_reach_their_requests_in_any_order() {
        let (client, mut server) = connect(quiet());
        let client = Arc::new(client);
        let first = tokio::spawn({
            let client = client.clone();
            async move { client.request("a", json!({}), Duration::from_secs(5)).await }
        });
        let a = server.next().await;
        let second = tokio::spawn({
            let client = client.clone();
            async move { client.request("b", json!({}), Duration::from_secs(5)).await }
        });
        let b = server.next().await;
        server.send(json!({"jsonrpc": "2.0", "id": b["id"], "result": "for b"})).await;
        server.send(json!({"jsonrpc": "2.0", "id": a["id"], "result": "for a"})).await;
        assert_eq!(first.await.unwrap(), Ok(json!("for a")));
        assert_eq!(second.await.unwrap(), Ok(json!("for b")));
    }

    #[tokio::test]
    async fn test_a_server_error_comes_back_as_one() {
        let (client, mut server) = connect(quiet());
        let request = tokio::spawn(async move { client.request("x", json!({}), Duration::from_secs(5)).await });
        let sent = server.next().await;
        server
            .send(json!({"jsonrpc": "2.0", "id": sent["id"], "error": {"code": -32601, "message": "no such method"}}))
            .await;
        assert_eq!(
            request.await.unwrap(),
            Err(LspError::Server { code: -32601, message: "no such method".to_string() })
        );
    }

    #[tokio::test]
    async fn test_a_request_that_times_out_is_cancelled_and_the_client_goes_on() {
        let (client, mut server) = connect(quiet());
        let result = client.request("slow", json!({}), Duration::from_millis(200)).await;
        assert_eq!(result, Err(LspError::Timeout(Duration::from_millis(200))));
        let sent = server.next().await;
        let cancel = server.next().await;
        assert_eq!(cancel["method"], "$/cancelRequest");
        assert_eq!(cancel["params"]["id"], sent["id"]);
        // A late answer is ignored, and the next request works.
        server.send(json!({"jsonrpc": "2.0", "id": sent["id"], "result": "late"})).await;
        let client = Arc::new(client);
        let next = tokio::spawn({
            let client = client.clone();
            async move { client.request("fast", json!({}), Duration::from_secs(5)).await }
        });
        let asked = server.next().await;
        server.send(json!({"jsonrpc": "2.0", "id": asked["id"], "result": "ok"})).await;
        assert_eq!(next.await.unwrap(), Ok(json!("ok")));
    }

    #[tokio::test]
    async fn test_the_servers_requests_and_notifications_go_to_the_handler() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let handler: Handler = {
            let seen = seen.clone();
            Arc::new(move |incoming| {
                seen.lock().unwrap().push(incoming.clone());
                match incoming {
                    Incoming::Request { method, .. } if method == "workspace/configuration" => json!([{"check": true}]),
                    _ => Value::Null,
                }
            })
        };
        let (_client, mut server) = connect(handler);
        server
            .send(json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": "file:///a", "diagnostics": []}}))
            .await;
        server
            .send(json!({"jsonrpc": "2.0", "id": 7, "method": "workspace/configuration", "params": {"items": [{}]}}))
            .await;
        let answer = server.next().await;
        assert_eq!(answer["id"], 7);
        assert_eq!(answer["result"], json!([{"check": true}]));
        let seen = seen.lock().unwrap();
        assert!(matches!(&seen[0], Incoming::Notification { method, .. } if method == "textDocument/publishDiagnostics"));
    }

    #[tokio::test]
    async fn test_when_the_server_goes_away_every_request_fails_closed() {
        let (client, server) = connect(quiet());
        let client = Arc::new(client);
        let waiting = tokio::spawn({
            let client = client.clone();
            async move { client.request("x", json!({}), Duration::from_secs(5)).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(server);
        assert_eq!(waiting.await.unwrap(), Err(LspError::Closed));
        assert!(client.is_closed());
        assert_eq!(client.request("y", json!({}), Duration::from_secs(5)).await, Err(LspError::Closed));
    }
}
