//! Talking to a running language server (SME-35): one session per server
//! pod, opened on first use. It keeps what the server has been told (open
//! files and their versions) and what it has said (diagnostics), and
//! answers its requests from the config (`workspace/configuration`).
//!
//! See SME-35's state model: a session is none → connecting → ready →
//! broken; a broken one is replaced on next use, a few times.

/// Only files here are served: server pods see nothing else.
pub const WORKSPACE: &str = "/workspace";

/// The project root for `path`: the nearest directory above it (up to
/// `/workspace`) that holds one of `markers`, else `/workspace`.
/// `has_marker` says whether a directory holds one.
pub fn project_root(path: &str, markers: &[String], has_marker: impl Fn(&str) -> bool) -> String {
    if !markers.is_empty() {
        let mut dir = std::path::Path::new(path).parent();
        while let Some(current) = dir {
            let current_str = current.to_str().unwrap_or(WORKSPACE);
            if !current_str.starts_with(WORKSPACE) {
                break;
            }
            if has_marker(current_str) {
                return current_str.to_string();
            }
            if current_str == WORKSPACE {
                break;
            }
            dir = current.parent();
        }
    }
    WORKSPACE.to_string()
}

/// `file:///workspace/...` for an absolute path, percent-encoding what a
/// URI can't hold.
pub fn file_uri(path: &str) -> String {
    // Everything but unreserved characters and `/`.
    const KEEP: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'/')
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    format!("file://{}", percent_encoding::utf8_percent_encode(path, KEEP))
}

/// The path in a `file://` URI, or `None` for any other.
pub fn uri_path(uri: &str) -> Option<String> {
    let path = uri.strip_prefix("file://")?;
    percent_encoding::percent_decode_str(path).decode_utf8().ok().map(|p| p.into_owned())
}

/// Whether `path` is a normalised absolute path under `/workspace`.
pub fn in_workspace(path: &str) -> bool {
    path.starts_with("/workspace/")
        && !path.split('/').any(|part| part == ".." || part == ".")
}

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Notify;

use crate::lsp::client::{Handler, Incoming, LspClient, LspError};
use crate::models::LanguageServerConfig;

/// How long `initialize` may take (a server that indexes first answers
/// only when it's ready to).
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(120);

/// What the server last said about a file's problems.
#[derive(Clone, Debug, Default)]
struct Published {
    version: Option<i64>,
    at: Option<Instant>,
    items: Vec<Value>,
}

/// State the client's handler updates as the server's messages arrive.
#[derive(Default)]
struct Heard {
    diagnostics: Mutex<HashMap<String, Published>>,
    /// Work-done progress under way (indexing, loading the project).
    progress: Mutex<HashSet<String>>,
    changed: Notify,
}

/// A file as the server was last told it.
#[derive(Clone, Debug)]
struct Document {
    version: i64,
    text: String,
}

/// Diagnostics for a file, and whether the list can be trusted as whole.
#[derive(Clone, Debug, PartialEq)]
pub struct FileDiagnostics {
    pub items: Vec<Value>,
    /// False when the server didn't answer in time or is still indexing.
    pub complete: bool,
}

/// One running server, as smelt talks to it.
pub struct Session {
    client: LspClient,
    pub config: LanguageServerConfig,
    capabilities: Value,
    roots: Mutex<Vec<String>>,
    documents: Mutex<HashMap<String, Document>>,
    heard: Arc<Heard>,
}

impl Session {
    /// Starts talking to a server on `reader`/`writer`: `initialize` with
    /// `root` as its first workspace folder, then `initialized` and its
    /// settings.
    pub async fn open<R, W>(reader: R, writer: W, config: LanguageServerConfig, root: &str) -> Result<Session, LspError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let heard = Arc::new(Heard::default());
        let client = LspClient::start(reader, writer, handler(heard.clone(), config.settings.clone(), root));
        let folder = json!({"uri": file_uri(root), "name": root.rsplit('/').next().unwrap_or("workspace")});
        let result = client
            .request(
                "initialize",
                json!({
                    "processId": null,
                    "clientInfo": {"name": "smelt"},
                    "rootUri": file_uri(root),
                    "rootPath": root,
                    "workspaceFolders": [folder],
                    "initializationOptions": config.initialization_options,
                    "capabilities": client_capabilities(),
                }),
                INITIALIZE_TIMEOUT,
            )
            .await?;
        client.notify("initialized", json!({})).await?;
        if let Some(settings) = &config.settings {
            client.notify("workspace/didChangeConfiguration", json!({"settings": settings})).await?;
        }
        Ok(Session {
            client,
            config,
            capabilities: result.get("capabilities").cloned().unwrap_or(Value::Null),
            roots: Mutex::new(vec![root.to_string()]),
            documents: Mutex::new(HashMap::new()),
            heard,
        })
    }

    pub fn is_closed(&self) -> bool {
        self.client.is_closed()
    }

    /// The server's capability at `path` (e.g. `definitionProvider`), when
    /// it has one; `false` and `null` count as not.
    pub fn capability(&self, path: &str) -> Option<&Value> {
        let value = path.split('.').try_fold(&self.capabilities, |v, key| v.get(key))?;
        match value {
            Value::Null | Value::Bool(false) => None,
            other => Some(other),
        }
    }

    /// Adds `root` as a workspace folder, if the server takes more than
    /// one and hasn't been told it yet.
    pub async fn add_root(&self, root: &str) -> Result<(), LspError> {
        let changes = self.capability("workspace.workspaceFolders.changeNotifications").is_some();
        {
            let mut roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
            if roots.iter().any(|r| r == root) || !changes {
                return Ok(());
            }
            roots.push(root.to_string());
        }
        let folder = json!({"uri": file_uri(root), "name": root.rsplit('/').next().unwrap_or("workspace")});
        self.client
            .notify("workspace/didChangeWorkspaceFolders", json!({"event": {"added": [folder], "removed": []}}))
            .await
    }

    /// Tells the server `path` now holds `text`: `didOpen` the first time,
    /// `didChange` after, then `didSave`. Returns the new version.
    pub async fn sync(&self, path: &str, text: &str) -> Result<i64, LspError> {
        let uri = file_uri(path);
        let previous = self.documents.lock().unwrap_or_else(|e| e.into_inner()).get(&uri).cloned();
        let version = match previous {
            None => {
                let language = self.language_id(path);
                self.client
                    .notify(
                        "textDocument/didOpen",
                        json!({"textDocument": {"uri": uri, "languageId": language, "version": 1, "text": text}}),
                    )
                    .await?;
                1
            }
            Some(document) if document.text == text => return Ok(document.version),
            Some(document) => {
                let version = document.version + 1;
                self.client
                    .notify(
                        "textDocument/didChange",
                        json!({"textDocument": {"uri": uri, "version": version}, "contentChanges": [{"text": text}]}),
                    )
                    .await?;
                version
            }
        };
        self.client.notify("textDocument/didSave", json!({"textDocument": {"uri": uri}, "text": text})).await?;
        self.documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(uri, Document { version, text: text.to_string() });
        Ok(version)
    }

    /// The text the server was last told `path` holds.
    pub fn synced_text(&self, path: &str) -> Option<String> {
        self.documents.lock().unwrap_or_else(|e| e.into_inner()).get(&file_uri(path)).map(|d| d.text.clone())
    }

    /// `path`'s diagnostics from version `version` on: waits up to `wait`
    /// for the server to publish them (or asks, if it takes pull requests).
    pub async fn diagnostics(&self, path: &str, version: i64, since: Instant, wait: Duration) -> FileDiagnostics {
        let uri = file_uri(path);
        let deadline = Instant::now() + wait;
        loop {
            let notified = self.heard.changed.notified();
            let current = self.heard.diagnostics.lock().unwrap_or_else(|e| e.into_inner()).get(&uri).cloned();
            if let Some(published) = current {
                let fresh = match published.version {
                    Some(v) => v >= version,
                    None => published.at.is_some_and(|at| at >= since),
                };
                if fresh {
                    return FileDiagnostics { items: published.items, complete: !self.is_indexing() };
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            if tokio::time::timeout(left, notified).await.is_err() {
                break;
            }
        }
        if self.capability("diagnosticProvider").is_some()
            && let Ok(report) = self
                .client
                .request("textDocument/diagnostic", json!({"textDocument": {"uri": uri}}), Duration::from_secs(10))
                .await
        {
            let items = report.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            return FileDiagnostics { items, complete: !self.is_indexing() };
        }
        FileDiagnostics { items: Vec::new(), complete: false }
    }

    /// Whether the server says it's still working (indexing, loading).
    pub fn is_indexing(&self) -> bool {
        !self.heard.progress.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, LspError> {
        self.client.request(method, params, timeout).await
    }

    fn language_id(&self, path: &str) -> String {
        let extension = path.rsplit_once('.').map(|(_, e)| e).unwrap_or_default();
        self.config.file_types.get(extension).cloned().unwrap_or_else(|| extension.to_string())
    }
}

/// What smelt tells servers it can do.
fn client_capabilities() -> Value {
    json!({
        "textDocument": {
            "synchronization": {"didSave": true},
            "publishDiagnostics": {"versionSupport": true, "relatedInformation": false},
            "diagnostic": {},
            "definition": {},
            "implementation": {},
            "references": {},
            "hover": {"contentFormat": ["markdown", "plaintext"]},
            "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
            "callHierarchy": {},
            "rename": {"prepareSupport": false},
        },
        "workspace": {
            "symbol": {},
            "workspaceFolders": true,
            "configuration": true,
            "didChangeWatchedFiles": {"dynamicRegistration": false},
            "workspaceEdit": {"documentChanges": true},
        },
        "window": {"workDoneProgress": true},
    })
}

/// Answers what the server sends: diagnostics and progress are kept, and
/// its requests get the config's settings or `null`.
fn handler(heard: Arc<Heard>, settings: Option<Value>, root: &str) -> Handler {
    let root = root.to_string();
    Arc::new(move |incoming| match incoming {
        Incoming::Notification { method, params } => {
            match method.as_str() {
                "textDocument/publishDiagnostics" => {
                    if let Some(uri) = params.get("uri").and_then(Value::as_str) {
                        let published = Published {
                            version: params.get("version").and_then(Value::as_i64),
                            at: Some(Instant::now()),
                            items: params.get("diagnostics").and_then(Value::as_array).cloned().unwrap_or_default(),
                        };
                        heard.diagnostics.lock().unwrap_or_else(|e| e.into_inner()).insert(uri.to_string(), published);
                        heard.changed.notify_waiters();
                    }
                }
                "$/progress" => {
                    let token = params.get("token").map(|t| t.to_string()).unwrap_or_default();
                    let kind = params.get("value").and_then(|v| v.get("kind")).and_then(Value::as_str);
                    let mut progress = heard.progress.lock().unwrap_or_else(|e| e.into_inner());
                    match kind {
                        Some("begin") => {
                            progress.insert(token);
                        }
                        Some("end") => {
                            progress.remove(&token);
                        }
                        _ => {}
                    }
                    drop(progress);
                    heard.changed.notify_waiters();
                }
                _ => {}
            }
            Value::Null
        }
        Incoming::Request { method, params } => match method.as_str() {
            "workspace/configuration" => {
                let items = params.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
                Value::Array(items.iter().map(|item| configuration_for(settings.as_ref(), item)).collect())
            }
            "workspace/workspaceFolders" => {
                json!([{"uri": file_uri(&root), "name": root.rsplit('/').next().unwrap_or("workspace")}])
            }
            _ => Value::Null,
        },
    })
}

/// The settings a `workspace/configuration` item asks for: its `section`
/// (dotted) in the config's settings, or all of them.
fn configuration_for(settings: Option<&Value>, item: &Value) -> Value {
    let Some(settings) = settings else { return Value::Null };
    match item.get("section").and_then(Value::as_str) {
        None | Some("") => settings.clone(),
        Some(section) => section
            .split('.')
            .try_fold(settings, |v, key| v.get(key))
            .cloned()
            .unwrap_or(Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::lsp::client::{frame, read_message};
    use tokio::io::{AsyncWriteExt, DuplexStream, duplex};

    /// A stand-in language server on the other end of in-memory pipes.
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

        /// Answers `initialize` with `capabilities`, and skips `initialized`.
        async fn handshake(&mut self, capabilities: Value) -> Value {
            let initialize = self.next().await;
            assert_eq!(initialize["method"], "initialize");
            self.send(json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"capabilities": capabilities}})).await;
            assert_eq!(self.next().await["method"], "initialized");
            initialize
        }
    }

    fn config(settings: Option<Value>) -> LanguageServerConfig {
        LanguageServerConfig {
            name: "stand-in".to_string(),
            command: "x".to_string(),
            file_types: [("rs".to_string(), "rust".to_string())].into(),
            initialization_options: Some(json!({"cargo": {"targetDir": true}})),
            settings,
            ..Default::default()
        }
    }

    fn connect() -> (DuplexStream, DuplexStream, StandIn) {
        let (client_out, from_client) = duplex(1 << 16);
        let (to_client, client_in) = duplex(1 << 16);
        (client_in, client_out, StandIn { from_client, to_client, buffer: Vec::new() })
    }

    #[tokio::test]
    async fn test_opening_says_where_the_project_is_and_passes_the_settings() {
        let (reader, writer, mut server) = connect();
        let settings = json!({"rust-analyzer": {"check": {"command": "clippy"}}});
        let opening = tokio::spawn(Session::open(reader, writer, config(Some(settings.clone())), "/workspace/app"));
        let initialize = server.handshake(json!({"definitionProvider": true})).await;
        assert_eq!(initialize["params"]["rootUri"], "file:///workspace/app");
        assert_eq!(initialize["params"]["initializationOptions"], json!({"cargo": {"targetDir": true}}));
        let configuration = server.next().await;
        assert_eq!(configuration["method"], "workspace/didChangeConfiguration");
        assert_eq!(configuration["params"]["settings"], settings);
        let session = opening.await.unwrap().expect("open");
        assert!(session.capability("definitionProvider").is_some());
        assert!(session.capability("renameProvider").is_none());

        // The server asks for a section of the settings.
        server
            .send(json!({"jsonrpc": "2.0", "id": 99, "method": "workspace/configuration",
                "params": {"items": [{"section": "rust-analyzer.check"}, {}]}}))
            .await;
        let answer = server.next().await;
        assert_eq!(answer["result"], json!([{"command": "clippy"}, settings]));
    }

    #[tokio::test]
    async fn test_a_file_is_opened_once_then_changed_with_the_next_version() {
        let (reader, writer, mut server) = connect();
        let opening = tokio::spawn(Session::open(reader, writer, config(None), "/workspace"));
        server.handshake(json!({})).await;
        let session = opening.await.unwrap().expect("open");

        assert_eq!(session.sync("/workspace/a.rs", "fn a() {}").await, Ok(1));
        let open = server.next().await;
        assert_eq!(open["method"], "textDocument/didOpen");
        assert_eq!(open["params"]["textDocument"]["languageId"], "rust");
        assert_eq!(server.next().await["method"], "textDocument/didSave");

        assert_eq!(session.sync("/workspace/a.rs", "fn a() { 1 }").await, Ok(2));
        let change = server.next().await;
        assert_eq!(change["method"], "textDocument/didChange");
        assert_eq!(change["params"]["textDocument"]["version"], 2);
        assert_eq!(change["params"]["contentChanges"][0]["text"], "fn a() { 1 }");
        assert_eq!(server.next().await["method"], "textDocument/didSave");

        // The same text again tells the server nothing new.
        assert_eq!(session.sync("/workspace/a.rs", "fn a() { 1 }").await, Ok(2));
        assert_eq!(session.synced_text("/workspace/a.rs").as_deref(), Some("fn a() { 1 }"));
    }

    #[tokio::test]
    async fn test_an_edits_diagnostics_are_the_ones_for_its_version() {
        let (reader, writer, mut server) = connect();
        let opening = tokio::spawn(Session::open(reader, writer, config(None), "/workspace"));
        server.handshake(json!({})).await;
        let session = Arc::new(opening.await.unwrap().expect("open"));

        let uri = "file:///workspace/a.rs";
        // Stale diagnostics for version 1 are already there.
        server
            .send(json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                "params": {"uri": uri, "version": 1, "diagnostics": [{"message": "old"}]}}))
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let since = Instant::now();
        let waiting = tokio::spawn({
            let session = session.clone();
            async move { session.diagnostics("/workspace/a.rs", 2, since, Duration::from_secs(3)).await }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        server
            .send(json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                "params": {"uri": uri, "version": 2, "diagnostics": [{"message": "new"}]}}))
            .await;
        let found = waiting.await.unwrap();
        assert_eq!(found.items, vec![json!({"message": "new"})]);
        assert!(found.complete);

        // Nothing published for a version: an empty list, marked incomplete.
        let nothing = session.diagnostics("/workspace/a.rs", 3, Instant::now(), Duration::from_millis(200)).await;
        assert_eq!(nothing, FileDiagnostics { items: vec![], complete: false });
    }

    #[tokio::test]
    async fn test_indexing_marks_diagnostics_incomplete() {
        let (reader, writer, mut server) = connect();
        let opening = tokio::spawn(Session::open(reader, writer, config(None), "/workspace"));
        server.handshake(json!({})).await;
        let session = opening.await.unwrap().expect("open");
        server
            .send(json!({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": "index", "value": {"kind": "begin", "title": "Indexing"}}}))
            .await;
        server
            .send(json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                "params": {"uri": "file:///workspace/a.rs", "version": 1, "diagnostics": []}}))
            .await;
        let found = session.diagnostics("/workspace/a.rs", 1, Instant::now(), Duration::from_secs(2)).await;
        assert!(!found.complete, "still indexing");
        server
            .send(json!({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": "index", "value": {"kind": "end"}}}))
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!session.is_indexing());
    }

    #[tokio::test]
    async fn test_a_second_project_is_added_once_when_the_server_takes_it() {
        let (reader, writer, mut server) = connect();
        let opening = tokio::spawn(Session::open(reader, writer, config(None), "/workspace/a"));
        server.handshake(json!({"workspace": {"workspaceFolders": {"supported": true, "changeNotifications": true}}})).await;
        let session = opening.await.unwrap().expect("open");
        session.add_root("/workspace/b").await.expect("add");
        session.add_root("/workspace/b").await.expect("again");
        session.add_root("/workspace/a").await.expect("the first");
        let added = server.next().await;
        assert_eq!(added["method"], "workspace/didChangeWorkspaceFolders");
        assert_eq!(added["params"]["event"]["added"][0]["uri"], "file:///workspace/b");
        // Nothing else was sent.
        assert!(tokio::time::timeout(Duration::from_millis(200), server.next()).await.is_err());
    }

    #[test]
    fn test_the_root_is_the_nearest_directory_with_a_marker() {
        let markers = vec!["Cargo.toml".to_string()];
        let has = |dir: &str| matches!(dir, "/workspace/app" | "/workspace/app/crates/core");
        assert_eq!(project_root("/workspace/app/crates/core/src/lib.rs", &markers, has), "/workspace/app/crates/core");
        assert_eq!(project_root("/workspace/app/src/main.rs", &markers, has), "/workspace/app");
        assert_eq!(project_root("/workspace/notes/todo.rs", &markers, has), "/workspace", "no marker: the workspace");
        // Never above /workspace, even if a marker were there.
        assert_eq!(project_root("/workspace/x.rs", &markers, |d| d == "/"), "/workspace");
    }

    #[test]
    fn test_paths_and_uris_round_trip() {
        assert_eq!(file_uri("/workspace/app/src/main.rs"), "file:///workspace/app/src/main.rs");
        assert_eq!(file_uri("/workspace/my app/a#b.rs"), "file:///workspace/my%20app/a%23b.rs");
        assert_eq!(uri_path("file:///workspace/my%20app/a%23b.rs").as_deref(), Some("/workspace/my app/a#b.rs"));
        assert_eq!(uri_path("https://example.com/x"), None);
    }

    #[test]
    fn test_only_workspace_files_are_served() {
        assert!(in_workspace("/workspace/app/src/main.rs"));
        assert!(!in_workspace("/workspace"));
        assert!(!in_workspace("/home/sandbox/x.rs"));
        assert!(!in_workspace("/workspace/../etc/passwd"));
        assert!(!in_workspace("workspace/x.rs"));
    }
}
