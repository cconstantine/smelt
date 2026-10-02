//! The web: fetching pages, HTTP requests, and previews of servers in the sandbox.

use serde_json::Value;
use sqlx::PgPool;

use super::Tool;
use super::server::required_str;
use crate::db;
use crate::anthropic::ToolDefinition;

pub(super) fn tools() -> Vec<Tool> {
    vec![
        Tool {
            def: ToolDefinition {
                name: "webfetch".to_string(),
                description: "Fetch a URL in a real browser (JS included — unlike a plain HTTP \
                               request, this handles JS-rendered pages) and return its \
                               rendered, readable text. Works with no pod created. http/https \
                               only; the resolved address (and every address any redirect or \
                               the page's own JS tries to reach) must be a real public address, \
                               not an internal/private one — except localhost (or 127.0.0.1), \
                               which means this conversation's sandbox pod: \
                               http://localhost:5173/ reaches a server listening on port 5173 \
                               there. Slower and heavier than http_request — prefer \
                               http_request for a JSON API or anything that doesn't need real \
                               page rendering."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "the http:// or https:// URL to fetch"}
                    },
                    "required": ["url"]
                }),
            },
            run: run!(|c| webfetch_tool(c.pool, c.conversation_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "sandbox_preview_url".to_string(),
                description: "Get a link the user can open in their own browser to see a server \
                               running in this conversation's sandbox pod on `port` (a dev \
                               server, a web app, an API's docs page). The sandbox panel shows \
                               the link too, so share it once the server is up. Says whether \
                               anything is listening on that port yet. Your own browser tools \
                               (webfetch, browser_navigate) reach the same server at \
                               http://localhost:<port>/: use that there, not this link. For a \
                               Docker container's port that isn't published, give the \
                               container's address as `host`; your browser tools reach it at \
                               http://<address>:<port>/."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "port": {"type": "integer", "description": "the port the server listens on inside the sandbox, e.g. 5173"},
                        "host": {"type": "string", "description": "a Docker container's address in the sandbox, from `docker inspect`, for a server running in that container; leave out for the sandbox itself or a port published with -p"}
                    },
                    "required": ["port"]
                }),
            },
            run: run!(|c| sandbox_preview_url_tool(c.pool, c.conversation_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "http_request".to_string(),
                description: "Make a plain HTTP request (like curl) — no browser, no JS \
                               execution, much cheaper than webfetch. Use this for a JSON API, \
                               a REST endpoint, or anything else that doesn't need a real \
                               rendered page; use webfetch instead for a page that needs JS to \
                               show its real content. A non-2xx status (404, 500, ...) is \
                               returned as normal data, not an error. http/https only; the \
                               resolved address (and any redirect target) must be a real public \
                               address, not an internal/private one."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "the http:// or https:// URL to request"},
                        "method": {"type": "string", "description": "defaults to GET, e.g. GET/POST/PUT/PATCH/DELETE"},
                        "headers": {"type": "object", "additionalProperties": {"type": "string"}, "description": "extra request headers"},
                        "body": {"type": "string", "description": "raw request body, e.g. a JSON string"}
                    },
                    "required": ["url"]
                }),
            },
            run: run!(|c| http_request_tool(c.input)),
        },
    ]
}

async fn webfetch_tool(pool: &PgPool, conversation_id: i64, input: &Value) -> Result<String, String> {
    let url = required_str(input, "url")?;
    let sandbox = crate::egress_proxy::sandbox_dial(pool.clone(), conversation_id);
    let result = crate::webfetch::fetch(&url, Some(sandbox)).await?;
    serde_json::to_string(&result).map_err(|e| e.to_string())
}

async fn http_request_tool(input: &Value) -> Result<String, String> {
    let url = required_str(input, "url")?;
    let method = input
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_string();
    let headers: Vec<(String, String)> = input
        .get("headers")
        .and_then(Value::as_object)
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let body = input.get("body").and_then(Value::as_str);
    let result = crate::http_request::request(&method, &url, &headers, body).await?;
    serde_json::to_string(&result).map_err(|e| e.to_string())
}

/// A link the user can open in their own browser for `port` in this
/// conversation's sandbox (SME-42), after checking something listens
/// there. Recorded on the pod, so the sandbox panel shows it.
pub(super) async fn sandbox_preview_url_tool(pool: &PgPool, conversation_id: i64, input: &Value) -> Result<String, String> {
    let port = input
        .get("port")
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
        .filter(|&p| p != 0)
        .ok_or("port must be a number from 1 to 65535")?;
    let host = match input.get("host") {
        None | Some(Value::Null) => crate::sandbox::PodHost::Localhost,
        Some(host) => host
            .as_str()
            .and_then(crate::docker_net::container_address)
            .map(crate::sandbox::PodHost::Container)
            .ok_or(
                "host must be a Docker container's address in the sandbox, as `docker inspect` \
                 shows it (in 172.20.0.0/14); leave it out for a server in the sandbox itself",
            )?,
    };
    if host == crate::sandbox::PodHost::Localhost {
        crate::sandbox::check_reachable_port(port).map_err(|e| e.to_string())?;
    }
    let pod_id = crate::sandbox::live_pod_id(pool, conversation_id)
        .await
        .map_err(|e| match e {
            crate::sandbox::TerminalError::NoPod => "This conversation has no running sandbox: \
                call create_pod first, then start the server in it."
                .to_string(),
            other => other.to_string(),
        })?;
    let template = crate::preview::configured_template()
        .map_err(|e| format!("Previews are off on this smelt: {e}"))?;
    let listening = crate::sandbox::pod_port_is_listening(pool, conversation_id, host, port)
        .await
        .map_err(|e| e.to_string())?;
    let stored_host = match host {
        crate::sandbox::PodHost::Localhost => String::new(),
        crate::sandbox::PodHost::Container(ip) => ip.to_string(),
    };
    let where_ = match host {
        crate::sandbox::PodHost::Localhost => format!("localhost:{port}"),
        crate::sandbox::PodHost::Container(ip) => format!("{ip}:{port}"),
    };
    let ports = db::add_pod_preview(pool, pod_id, &stored_host, port)
        .await
        .map_err(|e| e.to_string())?;
    crate::events::publish(
        conversation_id,
        crate::events::ConversationEvent::SandboxPreviewUpdate {
            pod_id,
            previews: crate::preview::preview_links(
                &template,
                conversation_id,
                &crate::preview::stored_previews(&ports),
            ),
        },
    );
    let note = if listening {
        format!(
            "The user can open this link in their own browser; the sandbox panel shows it too. \
             Your own browser tools reach the same server at http://{where_}/, so use \
             that address with them, not this link."
        )
    } else {
        format!(
            "Nothing is listening on {where_} in the sandbox yet, so the link won't load \
             until something does. Check the server started and which port it uses. The link \
             is saved in the sandbox panel either way."
        )
    };
    Ok(serde_json::json!({
        "url": template.url_for(conversation_id, host, port),
        "port": port,
        "listening": listening,
        "note": note,
    })
    .to_string())
}
