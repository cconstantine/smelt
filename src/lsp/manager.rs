//! What the model's tools call (SME-35): starting servers, running the
//! `lsp` tool's operations, and diagnostics after an edit. Everything
//! happens next to the conversation's sandbox: files are read (and a
//! rename's edits written) in the server's pod, which shares `/workspace`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::lsp::client::LspError;
use crate::lsp::ops::{self, Operation};
use crate::lsp::pods::{self, SandboxRef, ServerState};
use crate::lsp::session::{self, Session};
use crate::models::LanguageServerConfig;

/// How long one request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an edit waits for its diagnostics.
const EDIT_DIAGNOSTICS_WAIT: Duration = Duration::from_secs(3);
/// The largest file read for a server.
const MAX_FILE_BYTES: usize = 1024 * 1024;
/// A broken session is reopened at most this often.
const RECONNECTS: usize = 3;
const RECONNECT_WINDOW: Duration = Duration::from_secs(300);

/// The `lsp` tool's input.
#[derive(Clone, Debug, Default)]
pub struct OperationInput {
    pub path: Option<String>,
    pub line: Option<u32>,
    pub character: Option<u32>,
    pub query: Option<String>,
    pub new_name: Option<String>,
}

/// The conversation's running sandbox pod, as server pods need it.
pub async fn sandbox_ref(pool: &PgPool, client: &kube::Client, conversation_id: i64) -> Result<SandboxRef, String> {
    let pod_id = crate::sandbox::live_pod_id(pool, conversation_id)
        .await
        .map_err(|_| "No sandbox is running: create one with create_pod first.".to_string())?;
    let pod_name = crate::sandbox::kubernetes_pod_name(pod_id);
    let pod = crate::sandbox::pods_api(client)
        .get(&pod_name)
        .await
        .map_err(|e| format!("Couldn't find the sandbox pod: {e}"))?;
    Ok(SandboxRef {
        conversation_id,
        pod_id,
        pod_uid: pod.metadata.uid.clone().ok_or("the sandbox pod has no uid")?,
        node: pod
            .spec
            .as_ref()
            .and_then(|s| s.node_name.clone())
            .ok_or("the sandbox pod isn't on a node yet")?,
        pod_name,
    })
}

async fn configs(pool: &PgPool) -> Result<Vec<(LanguageServerConfig, String)>, String> {
    Ok(crate::db::list_language_servers(pool)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|s| (s.config, s.updated_at.to_string()))
        .collect())
}

/// The ready server pods next to `sandbox`, by server name.
async fn ready_servers(client: &kube::Client, sandbox: &SandboxRef) -> Result<Vec<String>, String> {
    Ok(pods::list_with(client, sandbox.conversation_id)
        .await?
        .into_iter()
        .filter(|p| p.state == ServerState::Ready && p.pod_name == pods::server_pod_name(sandbox.pod_id, &p.name))
        .map(|p| p.name)
        .collect())
}

/// The `lsp_servers` tool: every configured server, what it takes, and
/// whether it's running here.
pub async fn servers_summary(pool: &PgPool, client: &kube::Client, sandbox: Option<&SandboxRef>) -> Result<String, String> {
    let configs = configs(pool).await?;
    if configs.is_empty() {
        return Ok("No language servers are configured. The user adds them on the Language servers page.".to_string());
    }
    let running = match sandbox {
        Some(sandbox) => pods::list_with(client, sandbox.conversation_id)
            .await?
            .into_iter()
            .filter(|p| p.pod_name == pods::server_pod_name(sandbox.pod_id, &p.name))
            .collect(),
        None => Vec::new(),
    };
    let mut lines = Vec::new();
    for (config, version) in &configs {
        let types: Vec<String> = config.file_types.keys().map(|e| format!(".{e}")).collect();
        let state = match running.iter().find(|p| p.name == config.name) {
            _ if !config.enabled => "disabled".to_string(),
            None => "not running".to_string(),
            Some(pod) => {
                let mut state = match &pod.state {
                    ServerState::Ready => "running".to_string(),
                    ServerState::Installing => "starting".to_string(),
                    ServerState::Stopped(reason) => format!("stopped ({})", reason.as_deref().unwrap_or("exited")),
                };
                if pod.config_version != *version {
                    state.push_str("; its settings changed since it started: start_language_server again to restart it");
                }
                state
            }
        };
        lines.push(format!("{}: {} ({state})", config.name, types.join(" ")));
    }
    Ok(lines.join("\n"))
}

/// The `start_language_server` tool.
pub async fn start(pool: &PgPool, client: &kube::Client, sandbox: &SandboxRef, name: &str) -> Result<String, String> {
    let configs = configs(pool).await?;
    let Some((config, version)) = configs.iter().find(|(c, _)| c.name == name) else {
        let names: Vec<&str> = configs.iter().map(|(c, _)| c.name.as_str()).collect();
        return Err(format!("No language server named {name}. Configured: {}.", if names.is_empty() { "none".to_string() } else { names.join(", ") }));
    };
    if !config.enabled {
        return Err(format!("{name} is disabled on the Language servers page."));
    }
    // A pod started with older settings is restarted.
    let stale = pods::list_with(client, sandbox.conversation_id)
        .await?
        .into_iter()
        .any(|p| p.name == name && p.config_version != *version && p.pod_name == pods::server_pod_name(sandbox.pod_id, name));
    if stale {
        pods::stop_everywhere_in(client, sandbox.conversation_id, name).await?;
    }
    match pods::start_with(client, sandbox, config, version).await? {
        pods::Started::Started => Ok(format!(
            "{name} is running. It takes {}; use the lsp tool on those files, and edits to them come back with diagnostics.",
            config.file_types.keys().map(|e| format!(".{e}")).collect::<Vec<_>>().join(" ")
        )),
        pods::Started::AlreadyRunning => Ok(format!("{name} is already running.")),
    }
}

/// Sessions by server pod name, and when each was (re)opened.
static SESSIONS: std::sync::LazyLock<Mutex<HashMap<String, Arc<Session>>>> = std::sync::LazyLock::new(Default::default);
static OPENED: std::sync::LazyLock<Mutex<HashMap<String, Vec<Instant>>>> = std::sync::LazyLock::new(Default::default);

fn connect_lock(pod_name: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        std::sync::LazyLock::new(Default::default);
    LOCKS.lock().unwrap_or_else(|e| e.into_inner()).entry(pod_name.to_string()).or_default().clone()
}

/// The session with server pod `pod_name`, opened with `root` on first
/// use, and reopened (a few times) when it breaks.
async fn session_for(client: &kube::Client, pod_name: &str, config: &LanguageServerConfig, root: &str) -> Result<Arc<Session>, String> {
    let lock = connect_lock(pod_name);
    let _connecting = lock.lock().await;
    let existing = SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).get(pod_name).cloned();
    if let Some(session) = existing.filter(|s| !s.is_closed()) {
        session.add_root(root).await.map_err(|e| e.to_string())?;
        return Ok(session);
    }
    {
        let mut opened = OPENED.lock().unwrap_or_else(|e| e.into_inner());
        let times = opened.entry(pod_name.to_string()).or_default();
        times.retain(|t| t.elapsed() < RECONNECT_WINDOW);
        if times.len() >= RECONNECTS {
            return Err(format!(
                "{} stopped answering {RECONNECTS} times in {} minutes; start it again with start_language_server.",
                config.name,
                RECONNECT_WINDOW.as_secs() / 60
            ));
        }
        times.push(Instant::now());
    }
    let io = pods::open_stdio(client, pod_name, config).await?;
    let session = Session::open(io.stdout, io.stdin, config.clone(), root)
        .await
        .map_err(|e| format!("{} didn't start talking: {e}", config.name))?;
    let session = Arc::new(session);
    SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).insert(pod_name.to_string(), session.clone());
    Ok(session)
}

/// `path`'s contents, read in the server's pod.
async fn read_in_pod(client: &kube::Client, pod_name: &str, path: &str) -> Result<String, String> {
    let limit = MAX_FILE_BYTES.to_string();
    let result = crate::sandbox::exec_with(client, pod_name, "server", &["head", "-c", &limit, "--", path], None)
        .await
        .map_err(|e| e.to_string())?;
    if result.exit_code != 0 {
        return Err(format!("Couldn't read {path}: {}", result.stderr.trim()));
    }
    Ok(result.stdout)
}

/// Replaces `path` with `content` in the server's pod, if it still has the
/// SHA-256 `expected`: a file changed since it was read isn't overwritten.
async fn write_in_pod(client: &kube::Client, pod_name: &str, path: &str, content: &str, expected: &str) -> Result<(), String> {
    let script = r#"[ "$(sha256sum -- "$1" | cut -d' ' -f1)" = "$2" ] || { echo "$1 changed while it was being renamed" >&2; exit 3; }; cat > "$1""#;
    let result = crate::sandbox::exec_with(client, pod_name, "server", &["sh", "-c", script, "_", path, expected], Some(content.as_bytes()))
        .await
        .map_err(|e| e.to_string())?;
    if result.exit_code != 0 {
        return Err(result.stderr.trim().to_string());
    }
    Ok(())
}

fn sha256(text: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Where each of `markers` would be, in every directory from `path`'s up
/// to `/workspace`.
fn marker_candidates(path: &str, markers: &[String]) -> Vec<String> {
    let mut candidates = Vec::new();
    for dir in std::path::Path::new(path).ancestors().skip(1) {
        let dir = dir.to_string_lossy();
        if dir != session::WORKSPACE && !session::in_workspace(&dir) {
            break;
        }
        candidates.extend(markers.iter().map(|marker| format!("{dir}/{marker}")));
    }
    candidates
}

/// The project root for `path` among `markers`, checked in the pod.
async fn root_in_pod(client: &kube::Client, pod_name: &str, path: &str, markers: &[String]) -> String {
    if markers.is_empty() {
        return session::WORKSPACE.to_string();
    }
    let candidates = marker_candidates(path, markers);
    let mut argv = vec!["sh", "-c", r#"for f; do [ -e "$f" ] && echo "$f"; done; true"#, "_"];
    argv.extend(candidates.iter().map(String::as_str));
    let found: Vec<String> = match crate::sandbox::exec_with(client, pod_name, "server", &argv, None).await {
        Ok(result) => result.stdout.lines().map(str::to_string).collect(),
        Err(_) => Vec::new(),
    };
    session::project_root(path, markers, |dir| markers.iter().any(|m| found.contains(&format!("{dir}/{m}"))))
}

/// Every file path a result names (`uri`, `targetUri`), for reading their
/// lines.
fn paths_in(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if (key == "uri" || key == "targetUri")
                    && let Some(path) = value.as_str().and_then(session::uri_path)
                    && !out.contains(&path)
                {
                    out.push(path);
                } else {
                    paths_in(value, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| paths_in(item, out)),
        _ => {}
    }
}

/// The lines of the files `value` names (at most 20 files), read in the pod.
async fn source_lines(client: &kube::Client, pod_name: &str, value: &Value) -> HashMap<String, Vec<String>> {
    let mut paths = Vec::new();
    paths_in(value, &mut paths);
    let mut files = HashMap::new();
    for path in paths.into_iter().take(20) {
        if let Ok(text) = read_in_pod(client, pod_name, &path).await {
            files.insert(path, text.lines().map(str::to_string).collect());
        }
    }
    files
}

fn request_error(config: &LanguageServerConfig, error: LspError) -> String {
    match error {
        LspError::Timeout(_) => format!(
            "{} didn't answer within {}s; it may still be indexing. Try again shortly.",
            config.name,
            REQUEST_TIMEOUT.as_secs()
        ),
        other => format!("{}: {other}", config.name),
    }
}

/// The `lsp` tool.
pub async fn operate(
    pool: &PgPool,
    client: &kube::Client,
    sandbox: &SandboxRef,
    operation: Operation,
    input: &OperationInput,
) -> Result<String, String> {
    let all = configs(pool).await?;
    let config_list: Vec<LanguageServerConfig> = all.iter().map(|(c, _)| c.clone()).collect();
    let running = ready_servers(client, sandbox).await?;
    let path = input.path.clone();
    if operation.needs_position() && (path.is_none() || input.line.is_none()) {
        return Err(format!("{} needs a path and a line (1-based).", operation.name()));
    }
    let config = match &path {
        Some(path) => ops::choose_server(path, &config_list, &running)?.clone(),
        None if operation == Operation::WorkspaceSymbols => {
            let mut up = config_list.iter().filter(|c| running.contains(&c.name));
            match (up.next(), up.next()) {
                (Some(only), None) => only.clone(),
                (None, _) => return Err("No language server is running. Start one with start_language_server.".to_string()),
                _ => return Err("Several language servers are running: give a path, to pick the one for that kind of file.".to_string()),
            }
        }
        None => return Err(format!("{} needs a path.", operation.name())),
    };
    let pod_name = pods::server_pod_name(sandbox.pod_id, &config.name);
    let root = match &path {
        Some(path) => root_in_pod(client, &pod_name, path, &config.root_markers).await,
        None => session::WORKSPACE.to_string(),
    };
    let session = session_for(client, &pod_name, &config, &root).await?;
    if let Some(capability) = operation.capability()
        && session.capability(capability).is_none()
    {
        return Err(format!("{} doesn't support {}.", config.name, operation.name()));
    }

    // The file as it is on disk now, told to the server.
    let mut file = None;
    if let Some(path) = &path {
        let text = read_in_pod(client, &pod_name, path).await?;
        let version = session.sync(path, &text).await.map_err(|e| request_error(&config, e))?;
        file = Some((path.clone(), text, version));
    }
    let position = || -> Result<Value, String> {
        let (path, text, _) = file.as_ref().ok_or("needs a path")?;
        let line = input.line.ok_or_else(|| format!("{} needs a line and a character (1-based).", operation.name()))?;
        let character = input.character.unwrap_or(1);
        let line_text = text.lines().nth(line.saturating_sub(1) as usize).ok_or_else(|| format!("{path} has no line {line}."))?;
        Ok(json!({
            "textDocument": {"uri": session::file_uri(path)},
            "position": {"line": line - 1, "character": ops::to_utf16(line_text, character)},
        }))
    };

    let request = |method: &'static str, params: Value| {
        let session = session.clone();
        let config = config.clone();
        async move { session.request(method, params, REQUEST_TIMEOUT).await.map_err(|e| request_error(&config, e)) }
    };
    let format_locations = |result: Value, files: HashMap<String, Vec<String>>| {
        ops::format_locations(&result, &mut |p, l| files.get(p).and_then(|lines| lines.get(l as usize).cloned()))
    };

    match operation {
        Operation::Definition | Operation::Implementation | Operation::References => {
            let mut params = position()?;
            let method = match operation {
                Operation::Definition => "textDocument/definition",
                Operation::Implementation => "textDocument/implementation",
                _ => {
                    params["context"] = json!({"includeDeclaration": true});
                    "textDocument/references"
                }
            };
            let result = request(method, params).await?;
            let files = source_lines(client, &pod_name, &result).await;
            Ok(format_locations(result, files))
        }
        Operation::Hover => Ok(ops::format_hover(&request("textDocument/hover", position()?).await?)),
        Operation::DocumentSymbols => {
            let (path, _, _) = file.as_ref().expect("a path");
            let result = request("textDocument/documentSymbol", json!({"textDocument": {"uri": session::file_uri(path)}})).await?;
            Ok(ops::format_symbols(&result, &mut |_, _| None))
        }
        Operation::WorkspaceSymbols => {
            let query = input.query.clone().unwrap_or_default();
            let result = request("workspace/symbol", json!({"query": query})).await?;
            Ok(ops::format_symbols(&result, &mut |_, _| None))
        }
        Operation::Diagnostics => {
            let (path, text, version) = file.as_ref().expect("a path");
            let long_ago = Instant::now().checked_sub(Duration::from_secs(3600)).unwrap_or_else(Instant::now);
            let found = session.diagnostics(path, *version, long_ago, Duration::from_secs(5)).await;
            let lines: Vec<&str> = text.lines().collect();
            let mut listed = ops::format_diagnostics(&found.items, &mut |l| lines.get(l as usize).map(|s| s.to_string()), false);
            if found.items.is_empty() {
                listed = "No problems.".to_string();
            }
            if !found.complete {
                listed.push_str("\n(The server may still be indexing; the list may be incomplete.)");
            }
            Ok(listed)
        }
        Operation::IncomingCalls | Operation::OutgoingCalls => {
            let items = request("textDocument/prepareCallHierarchy", position()?).await?;
            let Some(item) = items.as_array().and_then(|i| i.first()).cloned() else {
                return Ok("Nothing callable there.".to_string());
            };
            let (method, key) = if operation == Operation::IncomingCalls {
                ("callHierarchy/incomingCalls", "from")
            } else {
                ("callHierarchy/outgoingCalls", "to")
            };
            let calls = request(method, json!({"item": item})).await?;
            let targets: Vec<Value> = calls
                .as_array()
                .map(|calls| {
                    calls
                        .iter()
                        .filter_map(|call| call.get(key))
                        .map(|target| json!({"uri": target["uri"], "range": target["selectionRange"], "name": target["name"]}))
                        .collect()
                })
                .unwrap_or_default();
            if targets.is_empty() {
                return Ok(format!("No {} calls.", if key == "from" { "incoming" } else { "outgoing" }));
            }
            let targets = Value::Array(targets);
            let files = source_lines(client, &pod_name, &targets).await;
            Ok(format_locations(targets, files))
        }
        Operation::Rename => {
            let new_name = input.new_name.clone().ok_or("rename needs a new_name.")?;
            let mut params = position()?;
            params["newName"] = json!(new_name);
            let edit = request("textDocument/rename", params).await?;
            let files = ops::workspace_edit_files(&edit)?;
            if files.is_empty() {
                return Ok("Nothing to rename there.".to_string());
            }
            // Every file first, so none is written if one can't be.
            let mut planned = Vec::new();
            for (path, edits) in &files {
                let current = read_in_pod(client, &pod_name, path).await?;
                if let Some(seen) = session.synced_text(path)
                    && seen != current
                {
                    return Err(format!("{path} changed since the server last saw it; nothing was renamed. Try again."));
                }
                let updated = ops::apply_text_edits(&current, edits)?;
                planned.push((path.clone(), current, updated, edits.len()));
            }
            let mut report = Vec::new();
            for (path, current, updated, count) in planned {
                write_in_pod(client, &pod_name, &path, &updated, &sha256(&current)).await?;
                let _ = session.sync(&path, &updated).await;
                report.push(format!("{path} ({count} edit{})", if count == 1 { "" } else { "s" }));
            }
            Ok(format!(
                "Renamed to {new_name} in {} file{}:\n{}\nRead a file again before editing it.",
                report.len(),
                if report.len() == 1 { "" } else { "s" },
                report.join("\n")
            ))
        }
    }
}

/// After `edit_file`/`write_file`: `path`'s errors and warnings from a
/// running server that takes it, or `None` (no server, or it couldn't say).
/// Never starts a server, and never fails the edit.
pub async fn diagnostics_after_edit(pool: &PgPool, conversation_id: i64, path: &str) -> Option<String> {
    let all = configs(pool).await.ok()?;
    let config_list: Vec<LanguageServerConfig> = all.into_iter().map(|(c, _)| c).collect();
    // Nothing takes this kind of file: no need to ask the cluster.
    let extension = std::path::Path::new(path).extension()?.to_str()?;
    if !config_list.iter().any(|c| c.enabled && c.file_types.contains_key(extension)) {
        return None;
    }
    let client = crate::sandbox::kube_client();
    let client = &client;
    let sandbox = &sandbox_ref(pool, client, conversation_id).await.ok()?;
    let running = ready_servers(client, sandbox).await.ok()?;
    let config = ops::choose_server(path, &config_list, &running).ok()?.clone();
    let pod_name = pods::server_pod_name(sandbox.pod_id, &config.name);
    let root = root_in_pod(client, &pod_name, path, &config.root_markers).await;
    let session = session_for(client, &pod_name, &config, &root).await.ok()?;
    let text = read_in_pod(client, &pod_name, path).await.ok()?;
    let since = Instant::now();
    let version = session.sync(path, &text).await.ok()?;
    let found = session.diagnostics(path, version, since, EDIT_DIAGNOSTICS_WAIT).await;
    let lines: Vec<&str> = text.lines().collect();
    let listed = ops::format_diagnostics(&found.items, &mut |l| lines.get(l as usize).map(|s| s.to_string()), true);
    let mut out = if listed.is_empty() {
        format!("{}: no errors or warnings.", config.name)
    } else {
        format!("{} reports:\n{listed}", config.name)
    };
    if !found.complete {
        out.push_str("\n(It may still be indexing; the list may be incomplete.)");
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_candidates_stop_at_the_workspace() {
        let markers = vec!["Cargo.toml".to_string(), ".git".to_string()];
        assert_eq!(
            marker_candidates("/workspace/app/src/main.rs", &markers),
            vec![
                "/workspace/app/src/Cargo.toml",
                "/workspace/app/src/.git",
                "/workspace/app/Cargo.toml",
                "/workspace/app/.git",
                "/workspace/Cargo.toml",
                "/workspace/.git",
            ]
        );
        assert!(marker_candidates("/etc/passwd", &markers).is_empty());
    }

    #[test]
    fn paths_in_finds_each_file_once() {
        let result = json!([
            {"uri": "file:///workspace/a.rs", "range": {}},
            {"targetUri": "file:///workspace/b.rs", "targetRange": {}},
            {"uri": "file:///workspace/a.rs", "range": {}},
            {"location": {"uri": "file:///workspace/c.rs"}},
        ]);
        let mut paths = Vec::new();
        paths_in(&result, &mut paths);
        assert_eq!(paths, vec!["/workspace/a.rs", "/workspace/b.rs", "/workspace/c.rs"]);
    }
}
