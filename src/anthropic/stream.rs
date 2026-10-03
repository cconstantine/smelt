//! Streaming client for the real Anthropic Messages API (`stream: true`).
//! Distinct from and unrelated to Dioxus's own `ServerEvents` transport
//! between the browser and this server — this module only talks to Anthropic.

use futures_util::StreamExt;
use serde_json::Value;

use super::types::{ContentBlock, CreateMessageRequest, TokenUsage};

/// The fully assembled result of one streamed Anthropic turn: every content
/// block (text and/or tool_use) in order, plus the `stop_reason` that says
/// whether this turn is final or wants a tool run (`"tool_use"`).
#[derive(Debug, Clone, PartialEq)]
pub struct StreamedTurn {
    pub content: Vec<ContentBlock>,
    pub stop_reason: String,
    /// Real usage for *this call* — `input_tokens`/cache fields from
    /// `message_start`, `output_tokens` from the last `message_delta` that
    /// carried one (overwritten each time it arrives, same "last one wins"
    /// treatment `stop_reason` itself already gets). See
    /// SME-18.
    pub usage: TokenUsage,
}

/// How a request authenticates to a provider (SME-72).
#[derive(Clone, PartialEq)]
pub enum Auth {
    /// An `x-api-key` header, as Anthropic (and local Ollama) expect.
    ApiKey(String),
    /// An `Authorization: Bearer` header, for gateways that want one.
    Bearer(String),
}

// By hand, so a secret never reaches a log line.
impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey(_) => f.write_str("ApiKey(..)"),
            Self::Bearer(_) => f.write_str("Bearer(..)"),
        }
    }
}

/// A model provider to send requests to: its base URL and credential,
/// from its `inference_providers` row (`providers::endpoint`).
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint {
    pub base_url: String,
    pub auth: Auth,
}

impl Endpoint {
    /// `path` (starting with `/`) under the base URL.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim_end_matches('/'))
    }

    /// The HTTP client for requests to this provider. It doesn't follow
    /// redirects: reqwest drops `Authorization` on a redirect to another
    /// host but not `x-api-key`, so following one could hand the key to a
    /// host it wasn't entered for. Anthropic-compatible APIs don't redirect.
    /// One client for every provider, so connections are reused.
    pub fn client(&self) -> Result<reqwest::Client, String> {
        static CLIENT: std::sync::LazyLock<Result<reqwest::Client, String>> =
            std::sync::LazyLock::new(|| {
                reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .map_err(|e| format!("couldn't set up the HTTP client: {e}"))
            });
        CLIENT.clone()
    }

    /// Adds the credential and the `anthropic-version` header every
    /// Anthropic-compatible request carries.
    pub fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let request = request.header("anthropic-version", "2023-06-01");
        match &self.auth {
            Auth::ApiKey(key) => request.header("x-api-key", key),
            Auth::Bearer(token) => request.header("Authorization", format!("Bearer {token}")),
        }
    }
}

/// A single interpreted Anthropic SSE payload, reduced to what
/// `stream_anthropic_message` needs to act on. Anthropic's stream carries
/// several event types (message_start, content_block_start,
/// content_block_delta, content_block_stop, message_delta, message_stop) —
/// only text/thinking deltas and errors matter for v1, everything else is
/// ignored (including `redacted_thinking` blocks — a rare safety-filtered
/// variant with a different, delta-less shape, not modeled here).
#[derive(Debug, Clone, PartialEq)]
pub enum StreamOutcome {
    TextDelta(String),
    ThinkingDelta(String),
    ThinkingSignatureDelta(String),
    ToolUseStart {
        id: String,
        name: String,
    },
    ToolUseInputDelta(String),
    BlockStop,
    /// `output_tokens` is `None` only if a `message_delta` event genuinely
    /// carried no `usage` field at all (defensive — real Anthropic
    /// responses always include one alongside `stop_reason`) — distinct
    /// from a real, explicit zero.
    StopReason {
        reason: String,
        output_tokens: Option<i64>,
    },
    /// `message_start`'s own `usage` — the authoritative input/cache token
    /// counts for this call. `output_tokens` here is a placeholder Anthropic
    /// sends before any output has streamed; `StopReason`'s own
    /// `output_tokens` (from `message_delta`, arriving later) is what
    /// actually finalizes it — see `stream_anthropic_message`.
    InitialUsage(TokenUsage),
    Error(String),
    Ignored,
}

/// Parse one decoded JSON payload from an Anthropic SSE `data:` line.
/// Pure and synchronous — unit-testable without any network access.
fn interpret_stream_event(value: &Value) -> StreamOutcome {
    match value.get("type").and_then(Value::as_str) {
        Some("message_start") => {
            let usage = value.get("message").and_then(|m| m.get("usage"));
            match usage.and_then(|u| serde_json::from_value::<TokenUsage>(u.clone()).ok()) {
                Some(usage) => StreamOutcome::InitialUsage(usage),
                None => StreamOutcome::Ignored,
            }
        }
        Some("content_block_start") => {
            let block = value.get("content_block");
            let is_tool_use =
                block.and_then(|b| b.get("type")).and_then(Value::as_str) == Some("tool_use");
            if is_tool_use {
                let id = block
                    .and_then(|b| b.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let name = block
                    .and_then(|b| b.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                StreamOutcome::ToolUseStart {
                    id: id.to_string(),
                    name: name.to_string(),
                }
            } else {
                StreamOutcome::Ignored
            }
        }
        Some("content_block_delta") => {
            match value
                .get("delta")
                .and_then(|d| d.get("type"))
                .and_then(Value::as_str)
            {
                Some("text_delta") => {
                    let text = value
                        .get("delta")
                        .and_then(|d| d.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    StreamOutcome::TextDelta(text.to_string())
                }
                Some("input_json_delta") => {
                    let partial_json = value
                        .get("delta")
                        .and_then(|d| d.get("partial_json"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    StreamOutcome::ToolUseInputDelta(partial_json.to_string())
                }
                Some("thinking_delta") => {
                    let thinking = value
                        .get("delta")
                        .and_then(|d| d.get("thinking"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    StreamOutcome::ThinkingDelta(thinking.to_string())
                }
                Some("signature_delta") => {
                    let signature = value
                        .get("delta")
                        .and_then(|d| d.get("signature"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    StreamOutcome::ThinkingSignatureDelta(signature.to_string())
                }
                _ => StreamOutcome::Ignored,
            }
        }
        Some("content_block_stop") => StreamOutcome::BlockStop,
        Some("message_delta") => match value
            .get("delta")
            .and_then(|d| d.get("stop_reason"))
            .and_then(Value::as_str)
        {
            Some(reason) => StreamOutcome::StopReason {
                reason: reason.to_string(),
                output_tokens: value
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_i64),
            },
            None => StreamOutcome::Ignored,
        },
        Some("error") => {
            let message = value
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unknown error from Anthropic");
            StreamOutcome::Error(message.to_string())
        }
        _ => StreamOutcome::Ignored,
    }
}

/// Accumulates one in-progress content block across its
/// `content_block_start` → `content_block_delta`* → `content_block_stop`
/// events. Anthropic emits blocks sequentially, never interleaved, so
/// tracking a single "current" accumulator (rather than a map keyed by
/// block index) is sufficient.
enum PartialBlock {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        partial_json: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
}

impl PartialBlock {
    fn finalize(self) -> Result<ContentBlock, String> {
        match self {
            PartialBlock::Text(text) => Ok(ContentBlock::Text { text }),
            PartialBlock::ToolUse {
                id,
                name,
                partial_json,
            } => {
                let input = if partial_json.is_empty() {
                    Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_str(&partial_json).map_err(|e| {
                        format!("failed to parse tool_use input JSON for {name}: {e}")
                    })?
                };
                Ok(ContentBlock::ToolUse { id, name, input })
            }
            PartialBlock::Thinking {
                thinking,
                signature,
            } => Ok(ContentBlock::Thinking {
                thinking,
                signature,
            }),
        }
    }
}

/// Bound on how long we'll wait for the *next* chunk from Anthropic before
/// giving up — a dropped upstream connection must not hang the caller's
/// background task forever. Applied per-chunk, not to the whole stream, since
/// a long response is expected to take a while overall.
const CHUNK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Bound on how long we'll wait for Anthropic to even start responding —
/// TCP connect through receiving the response headers — before giving up.
/// Distinct from `CHUNK_TIMEOUT`, which only bounds the gap between chunks
/// once a response is already streaming; this call had no bound at all
/// before (see SME-8's retrospective: a stalled connect
/// here is indistinguishable from the caller just hanging forever, so a
/// stalled connect looked like a stuck turn rather than a visible error).
///
/// Raised from the original 90s: a local Ollama server (a supported
/// provider, not just the real Anthropic API) can take
/// several minutes to cold-load a model into memory before it sends
/// anything back at all, and that wait genuinely belongs here rather than
/// in `CHUNK_TIMEOUT` — nothing has started streaming yet. Still just a
/// backstop against a truly dead connection, not a real per-request budget.
const RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// How a request replays the conversation's earlier thinking (SME-93).
/// Under Anthropic's preserved thinking, a thinking block is bound to the
/// `system` prompt, the `tools` and every message before it; smelt
/// rebuilds the system prompt each turn, and accounts created on or after
/// 2026-08-31 refuse a request replaying a block whose prefix changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Binding {
    /// The request as built: what every provider gets until it refuses it.
    AsIs,
    /// `thinking.block_binding.prefix_mismatch_behavior: "drop_block"` under
    /// the controls beta: the API drops the blocks it can't accept and
    /// keeps the rest. Needs a `thinking` object, so only with thinking on.
    DropBlock,
    /// The history without its thinking blocks, which every
    /// Anthropic-compatible server accepts.
    Strip,
}

/// The beta that lets a request set `block_binding`.
const BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";

/// Providers that accepted drop-block after refusing a request as built,
/// keyed by `provider_key`. Enforcement is per account, so one
/// provider's answer holds for all its conversations; a restart forgets it
/// and costs one more refused request per provider.
static RECOVERY: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<u64, Binding>>> =
    std::sync::LazyLock::new(Default::default);

/// A provider's identity for `RECOVERY`: its address and credential,
/// hashed so the credential isn't kept. A new key may be another account.
fn provider_key(endpoint: &Endpoint) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    endpoint.base_url.hash(&mut hasher);
    match &endpoint.auth {
        Auth::ApiKey(key) => ("key", key).hash(&mut hasher),
        Auth::Bearer(token) => ("bearer", token).hash(&mut hasher),
    }
    hasher.finish()
}

/// The form to send `request` in first: drop-block when `endpoint` took it
/// before and the request has thinking on (it needs a `thinking` object),
/// otherwise as built.
fn binding_for(endpoint: &Endpoint, request: &CreateMessageRequest) -> Binding {
    let remembered = RECOVERY
        .lock()
        .map(|known| known.get(&provider_key(endpoint)).copied())
        .unwrap_or(None)
        .unwrap_or(Binding::AsIs);
    match remembered {
        Binding::DropBlock if request.thinking.is_some() => Binding::DropBlock,
        _ => Binding::AsIs,
    }
}

fn remember_binding(endpoint: &Endpoint, binding: Binding) {
    if let Ok(mut known) = RECOVERY.lock() {
        known.insert(provider_key(endpoint), binding);
    }
}

/// Whether a 400's body is the preserved-thinking refusal of a replayed
/// block ("... bound to a different conversation ..."), as opposed to a
/// tampered signature or any other error.
fn is_binding_mismatch(body: &str) -> bool {
    provider_error_message(body).contains("bound to a different conversation")
}

/// The next form to try after `binding` was refused with a 400 whose body
/// is `body`, or `None` when another try won't help. Each step is tried
/// once: as built, then drop-block (with thinking on), then stripped.
fn next_binding(binding: Binding, request: &CreateMessageRequest, body: &str) -> Option<Binding> {
    match binding {
        Binding::AsIs if is_binding_mismatch(body) && request.thinking.is_some() => Some(Binding::DropBlock),
        Binding::AsIs if is_binding_mismatch(body) => Some(Binding::Strip),
        // A server that doesn't know the controls beta refuses the field
        // itself ("block_binding: Extra inputs are not permitted"). Any
        // other 400 isn't about the replayed thinking, and stripping it
        // would only hide the real error.
        Binding::DropBlock if is_binding_mismatch(body) || body.contains("block_binding") => Some(Binding::Strip),
        _ => None,
    }
}

/// How many thinking blocks the API says it dropped before the model saw
/// them, from a `message_start` event's `input_transformations` (sent
/// under the controls beta). Entries of any other type are ignored, as
/// the docs ask: later checks add values.
fn dropped_thinking(event: &Value) -> usize {
    if event.get("type").and_then(Value::as_str) != Some("message_start") {
        return 0;
    }
    event
        .pointer("/message/input_transformations")
        .and_then(Value::as_array)
        .map_or(0, |entries| entries.iter().filter(|e| e["type"] == "thinking_dropped").count())
}

/// `request`'s JSON body in the form `binding` asks for.
fn request_body(request: &CreateMessageRequest, binding: Binding) -> Result<Value, String> {
    let mut body = match binding {
        Binding::Strip => {
            let mut stripped = request.clone();
            for message in &mut stripped.messages {
                message.content = super::types::strip_thinking(std::mem::take(&mut message.content));
            }
            serde_json::to_value(&stripped)
        }
        _ => serde_json::to_value(request),
    }
    .map_err(|e| format!("couldn't encode the model request: {e}"))?;
    if binding == Binding::DropBlock
        && let Some(thinking) = body.get_mut("thinking").and_then(Value::as_object_mut)
    {
        thinking.insert(
            "block_binding".to_string(),
            serde_json::json!({"prefix_mismatch_behavior": "drop_block"}),
        );
    }
    Ok(body)
}

/// Sends the request and waits for Anthropic's response headers, bounded by
/// `response_timeout` — factored out from `stream_anthropic_message` so a
/// test can exercise the timeout with a short duration instead of the real
/// `RESPONSE_TIMEOUT`.
async fn send_and_await_response(
    endpoint: &Endpoint,
    request: &CreateMessageRequest,
    binding: Binding,
    response_timeout: std::time::Duration,
) -> Result<reqwest::Response, String> {
    let mut client = endpoint
        .authorize(endpoint.client()?.post(endpoint.url("/v1/messages")))
        .json(&request_body(request, binding)?);
    if binding == Binding::DropBlock {
        client = client.header("anthropic-beta", BINDING_BETA);
    }
    tokio::time::timeout(response_timeout, client.send())
        .await
        .map_err(|_| "timed out waiting for Anthropic to respond".to_string())?
        .map_err(|e| format!("The model request failed: {e}"))
}

/// The human-readable part of an error response: Anthropic's own
/// `error.message`, unwrapped once more when a router relays a provider's
/// JSON error inside it (`{"error": "…"}`). Falls back to the raw body.
fn provider_error_message(body: &str) -> String {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.pointer("/error/message")?.as_str().map(str::to_string));
    let Some(message) = message else {
        return body.trim().to_string();
    };
    let nested = serde_json::from_str::<serde_json::Value>(&message).ok().and_then(|v| {
        v.get("error")
            .or_else(|| v.get("message"))?
            .as_str()
            .map(str::to_string)
    });
    nested.unwrap_or(message).trim().to_string()
}

/// Statuses worth another try: rate limiting and a provider that's
/// temporarily unavailable or overloaded.
fn is_transient_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 503 | 529)
}

/// Waits between attempts after a transient status — three attempts in all.
#[cfg(not(test))]
const RETRY_DELAYS: [std::time::Duration; 2] = [
    std::time::Duration::from_secs(1),
    std::time::Duration::from_secs(3),
];
#[cfg(test)]
const RETRY_DELAYS: [std::time::Duration; 2] = [
    std::time::Duration::from_millis(10),
    std::time::Duration::from_millis(10),
];

/// Stream a message from the real Anthropic API, calling `on_delta` for each
/// text chunk as it arrives (live typing effect) and returning every content
/// block — text and/or tool_use — plus the turn's `stop_reason` once the
/// stream ends. Tool-use blocks accumulate silently; `on_delta` only ever
/// fires for text. `request.stream` should be `true`.
pub async fn stream_anthropic_message(
    endpoint: &Endpoint,
    request: &CreateMessageRequest,
    mut on_delta: impl FnMut(&str),
) -> Result<StreamedTurn, String> {
    // Retried only here, before anything has streamed: nothing has been
    // shown to the viewer yet, so a retry is invisible to them.
    let mut attempt = 0;
    let mut binding = binding_for(endpoint, request);
    let response = loop {
        let response = send_and_await_response(endpoint, request, binding, RESPONSE_TIMEOUT).await?;
        let status = response.status();
        if status.is_success() {
            // Only drop-block is remembered. A stripped request is sent only
            // after the API's own binding refusal, never ahead of one: the
            // memory covers every model on the provider, and a model that
            // doesn't run the check may require the last assistant turn's
            // thinking block.
            if binding == Binding::DropBlock {
                remember_binding(endpoint, binding);
            }
            break response;
        }
        if is_transient_status(status) && attempt < RETRY_DELAYS.len() {
            tracing::warn!(%status, attempt, "model provider unavailable; retrying");
            tokio::time::sleep(RETRY_DELAYS[attempt]).await;
            attempt += 1;
            continue;
        }
        let body = response.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::BAD_REQUEST
            && let Some(next) = next_binding(binding, request, &body)
        {
            tracing::warn!(
                from = ?binding,
                to = ?next,
                error = %provider_error_message(&body),
                "model provider refused the replayed thinking; retrying"
            );
            binding = next;
            continue;
        }
        return Err(format!(
            "model provider error {status}: {}",
            provider_error_message(&body)
        ));
    };

    let mut byte_stream = response.bytes_stream();
    let mut line_buffer = String::new();
    let mut content: Vec<ContentBlock> = Vec::new();
    let mut current: Option<PartialBlock> = None;
    let mut stop_reason = String::new();
    let mut usage = TokenUsage::default();

    loop {
        let next = tokio::time::timeout(CHUNK_TIMEOUT, byte_stream.next())
            .await
            .map_err(|_| "timed out waiting for the next chunk from Anthropic".to_string())?;

        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|e| format!("error reading Anthropic response stream: {e}"))?;
        line_buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(newline_pos) = line_buffer.find('\n') {
            let line: String = line_buffer.drain(..=newline_pos).collect();
            let line = line.trim();
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            let dropped = dropped_thinking(&value);
            if dropped > 0 {
                tracing::info!(dropped, "the model provider dropped replayed thinking it couldn't accept");
            }
            match interpret_stream_event(&value) {
                StreamOutcome::TextDelta(text) => {
                    on_delta(&text);
                    match &mut current {
                        Some(PartialBlock::Text(existing)) => existing.push_str(&text),
                        _ => current = Some(PartialBlock::Text(text)),
                    }
                }
                // Not passed to `on_delta` — thinking is never part of the
                // live-typed reply, only shown (collapsed) once the full
                // block lands with the finished message. No live "typing"
                // effect for it, unlike text.
                StreamOutcome::ThinkingDelta(thinking) => match &mut current {
                    Some(PartialBlock::Thinking {
                        thinking: existing, ..
                    }) => existing.push_str(&thinking),
                    _ => {
                        current = Some(PartialBlock::Thinking {
                            thinking,
                            signature: String::new(),
                        })
                    }
                },
                StreamOutcome::ThinkingSignatureDelta(signature) => {
                    if let Some(PartialBlock::Thinking {
                        signature: existing,
                        ..
                    }) = &mut current
                    {
                        existing.push_str(&signature);
                    }
                }
                StreamOutcome::ToolUseStart { id, name } => {
                    current = Some(PartialBlock::ToolUse {
                        id,
                        name,
                        partial_json: String::new(),
                    });
                }
                StreamOutcome::ToolUseInputDelta(partial_json) => {
                    if let Some(PartialBlock::ToolUse {
                        partial_json: existing,
                        ..
                    }) = &mut current
                    {
                        existing.push_str(&partial_json);
                    }
                }
                StreamOutcome::BlockStop => {
                    if let Some(block) = current.take() {
                        content.push(block.finalize()?);
                    }
                }
                StreamOutcome::StopReason {
                    reason,
                    output_tokens,
                } => {
                    stop_reason = reason;
                    if let Some(output_tokens) = output_tokens {
                        usage.output_tokens = output_tokens;
                    }
                }
                StreamOutcome::InitialUsage(initial) => {
                    usage.input_tokens = initial.input_tokens;
                    usage.cache_creation_input_tokens = initial.cache_creation_input_tokens;
                    usage.cache_read_input_tokens = initial.cache_read_input_tokens;
                }
                StreamOutcome::Error(message) => return Err(message),
                StreamOutcome::Ignored => {}
            }
        }
    }

    if let Some(block) = current.take() {
        content.push(block.finalize()?);
    }

    Ok(StreamedTurn {
        content,
        stop_reason,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SME-40 F12: a base URL ending in `/` (as the dev `.env`'s does)
    /// produced `//v1/messages`.
    #[test]
    fn test_messages_url_ignores_a_trailing_slash() {
        let url = |base: &str| {
            Endpoint { base_url: base.to_string(), auth: Auth::ApiKey(String::new()) }.url("/v1/messages")
        };
        assert_eq!(url("https://api.anthropic.com"), "https://api.anthropic.com/v1/messages");
        assert_eq!(url("https://gateway.example/"), "https://gateway.example/v1/messages");
        assert_eq!(url("https://gateway.example/proxy/"), "https://gateway.example/proxy/v1/messages");
    }

    /// SME-72 review 2: reqwest drops `Authorization` on a cross-host
    /// redirect but not `x-api-key`, so a provider that redirects could
    /// hand the key to another host. Provider requests don't follow
    /// redirects.
    #[tokio::test]
    async fn test_a_redirect_never_carries_the_key_to_another_host() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let elsewhere = listener.local_addr().expect("addr");
        let reached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = reached.clone();
        let app = axum::Router::new().fallback(move || {
            let flag = flag.clone();
            async move {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                "gotcha"
            }
        });
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let provider = listener.local_addr().expect("addr");
        let app = axum::Router::new().fallback(move || async move {
            axum::response::Redirect::temporary(&format!("http://localhost:{}/steal", elsewhere.port()))
        });
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let endpoint = Endpoint {
            base_url: format!("http://{provider}"),
            auth: Auth::ApiKey("sk-secret".to_string()),
        };
        let request = CreateMessageRequest {
            model: "m".to_string(),
            max_tokens: 10,
            system: None,
            messages: vec![],
            stream: true,
            tools: vec![],
            thinking: None,
        };
        let result = stream_anthropic_message(&endpoint, &request, |_| {}).await;
        assert!(result.is_err(), "a redirect isn't a reply");
        assert!(!reached.load(std::sync::atomic::Ordering::SeqCst), "the redirect was followed");
    }

    /// A credential never reaches a log line through `{:?}`.
    #[test]
    fn test_an_endpoints_debug_hides_its_secret() {
        let endpoint = Endpoint {
            base_url: "https://api.anthropic.com".to_string(),
            auth: Auth::Bearer("sk-very-secret".to_string()),
        };
        let shown = format!("{endpoint:?}");
        assert!(!shown.contains("sk-very-secret"), "{shown}");
        assert!(shown.contains("Bearer"), "{shown}");
    }

    #[test]
    fn test_interpret_text_delta_extracts_text() {
        let value = serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "Hello"}
        });
        assert_eq!(
            interpret_stream_event(&value),
            StreamOutcome::TextDelta("Hello".to_string())
        );
    }

    #[test]
    fn test_interpret_unrecognized_delta_type_is_ignored() {
        let value = serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "some_future_delta_type", "thinking": "..."}
        });
        assert_eq!(interpret_stream_event(&value), StreamOutcome::Ignored);
    }

    #[test]
    fn test_interpret_thinking_delta_extracts_thinking_text() {
        let value = serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "thinking_delta", "thinking": "Let me consider..."}
        });
        assert_eq!(
            interpret_stream_event(&value),
            StreamOutcome::ThinkingDelta("Let me consider...".to_string())
        );
    }

    #[test]
    fn test_interpret_signature_delta_extracts_signature() {
        let value = serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "signature_delta", "signature": "abc123"}
        });
        assert_eq!(
            interpret_stream_event(&value),
            StreamOutcome::ThinkingSignatureDelta("abc123".to_string())
        );
    }

    #[test]
    fn test_interpret_message_stop_is_ignored() {
        let value = serde_json::json!({"type": "message_stop"});
        assert_eq!(interpret_stream_event(&value), StreamOutcome::Ignored);
    }

    #[test]
    fn test_interpret_error_event_extracts_message() {
        let value = serde_json::json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "Overloaded"}
        });
        assert_eq!(
            interpret_stream_event(&value),
            StreamOutcome::Error("Overloaded".to_string())
        );
    }

    #[test]
    fn test_interpret_content_block_start_tool_use_captures_id_and_name() {
        let value = serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_01", "name": "add", "input": {}}
        });
        assert_eq!(
            interpret_stream_event(&value),
            StreamOutcome::ToolUseStart {
                id: "toolu_01".to_string(),
                name: "add".to_string()
            }
        );
    }

    #[test]
    fn test_interpret_content_block_start_text_is_ignored() {
        let value = serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""}
        });
        assert_eq!(interpret_stream_event(&value), StreamOutcome::Ignored);
    }

    #[test]
    fn test_interpret_input_json_delta_extracts_partial_json() {
        let value = serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "{\"a\":"}
        });
        assert_eq!(
            interpret_stream_event(&value),
            StreamOutcome::ToolUseInputDelta("{\"a\":".to_string())
        );
    }

    #[test]
    fn test_interpret_content_block_stop_returns_block_stop() {
        let value = serde_json::json!({"type": "content_block_stop", "index": 0});
        assert_eq!(interpret_stream_event(&value), StreamOutcome::BlockStop);
    }

    #[test]
    fn test_interpret_message_delta_extracts_stop_reason_and_output_tokens() {
        let value = serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": "tool_use", "stop_sequence": null},
            "usage": {"output_tokens": 12}
        });
        assert_eq!(
            interpret_stream_event(&value),
            StreamOutcome::StopReason {
                reason: "tool_use".to_string(),
                output_tokens: Some(12)
            }
        );
    }

    #[test]
    fn test_interpret_message_delta_without_usage_reports_no_output_tokens() {
        let value = serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null}
        });
        assert_eq!(
            interpret_stream_event(&value),
            StreamOutcome::StopReason {
                reason: "end_turn".to_string(),
                output_tokens: None
            }
        );
    }

    #[test]
    fn test_interpret_message_start_extracts_initial_usage() {
        let value = serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_01",
                "usage": {
                    "input_tokens": 2095,
                    "cache_creation_input_tokens": 10,
                    "cache_read_input_tokens": 5,
                    "output_tokens": 3
                }
            }
        });
        assert_eq!(
            interpret_stream_event(&value),
            StreamOutcome::InitialUsage(TokenUsage {
                input_tokens: 2095,
                output_tokens: 3,
                cache_creation_input_tokens: 10,
                cache_read_input_tokens: 5,
            })
        );
    }

    #[test]
    fn test_interpret_message_start_without_usage_is_ignored() {
        let value = serde_json::json!({"type": "message_start", "message": {"id": "msg_01"}});
        assert_eq!(interpret_stream_event(&value), StreamOutcome::Ignored);
    }

    fn test_endpoint(addr: std::net::SocketAddr) -> Endpoint {
        Endpoint {
            base_url: format!("http://{addr}"),
            auth: Auth::ApiKey("test-key".to_string()),
        }
    }

    /// Spins up a throwaway mock upstream returning `mock_body` verbatim
    /// and runs `stream_anthropic_message` against it.
    async fn run_against_mock_upstream(
        mock_body: &'static str,
        on_delta: impl FnMut(&str),
    ) -> Result<StreamedTurn, String> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move || async move {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    mock_body,
                )
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let request = CreateMessageRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 100,
            system: None,
            messages: vec![],
            stream: true,
            tools: vec![],
            thinking: None,
        };

        stream_anthropic_message(&test_endpoint(addr), &request, on_delta).await
    }

    /// The body a paused provider actually returned during the bug bash,
    /// relayed through an Anthropic-compatible router: the provider's
    /// message is JSON inside the Anthropic error's own `message`.
    const PAUSED_PROVIDER_BODY: &str = r#"{"type":"error","error":{"type":"api_error","message":"{\"error\":\"Provider 'featherless-ai' is currently failing for model 'Qwen/Qwen3.8-27B' and has been paused by the circuit breaker. Retry later or use another provider.\"}"}}"#;

    const OK_BODY: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\"}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    /// Serves `responses` in order (the last one repeats) and returns the
    /// outcome plus how many requests were made.
    async fn run_against_responses(
        responses: Vec<(u16, &'static str)>,
    ) -> (Result<StreamedTurn, String>, usize) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let served = count.clone();
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move || {
                let responses = responses.clone();
                let served = served.clone();
                async move {
                    let i = served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let (status, body) = responses[i.min(responses.len() - 1)];
                    let content_type = if status == 200 { "text/event-stream" } else { "application/json" };
                    (
                        axum::http::StatusCode::from_u16(status).expect("valid status"),
                        [(axum::http::header::CONTENT_TYPE, content_type)],
                        body,
                    )
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let request = CreateMessageRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 100,
            system: None,
            messages: vec![],
            stream: true,
            tools: vec![],
            thinking: None,
        };
        let result = stream_anthropic_message(&test_endpoint(addr), &request, |_| {}).await;
        (result, count.load(std::sync::atomic::Ordering::SeqCst))
    }

    #[tokio::test]
    async fn test_a_transient_provider_error_is_retried() {
        let (result, requests) =
            run_against_responses(vec![(503, PAUSED_PROVIDER_BODY), (200, OK_BODY)]).await;
        assert!(result.is_ok(), "should recover on retry: {result:?}");
        assert_eq!(requests, 2);
    }

    #[tokio::test]
    async fn test_a_client_error_is_not_retried() {
        let (result, requests) =
            run_against_responses(vec![(400, r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#)]).await;
        assert!(result.is_err());
        assert_eq!(requests, 1, "a 400 won't get better by retrying");
    }

    /// The 400 an account enforcing preserved thinking returns when a
    /// replayed thinking block's prefix changed (SME-93). Its wording is
    /// the Preserved thinking page's; no enforced account was available
    /// to capture a real one.
    const BINDING_BODY: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.1.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation. Remove the block, or set `thinking.block_binding.prefix_mismatch_behavior` to \"drop_block\". That setting requires the `thinking-binding-controls-2026-08-01` value in the `anthropic-beta` header."}}"#;

    /// What a server without the controls beta says to `block_binding`.
    const EXTRA_INPUTS_BODY: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"thinking.adaptive.block_binding: Extra inputs are not permitted"}}"#;

    const BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";

    /// One request a recording upstream received.
    #[derive(Debug, Clone)]
    struct Seen {
        beta: Option<String>,
        body: Value,
    }

    impl Seen {
        fn drop_block(&self) -> Option<&str> {
            self.body.pointer("/thinking/block_binding/prefix_mismatch_behavior")?.as_str()
        }

        fn thinking_blocks(&self) -> usize {
            self.body["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|m| m["content"].as_array().into_iter().flatten())
                .filter(|b| b["type"] == "thinking")
                .count()
        }

        fn has_binding_beta(&self) -> bool {
            self.beta.as_deref().is_some_and(|b| b.split(',').any(|v| v.trim() == BINDING_BETA))
        }
    }

    /// An upstream serving `responses` in order (the last one repeats),
    /// recording each request; returns its endpoint and the record.
    async fn recording_upstream(
        responses: Vec<(u16, &'static str)>,
    ) -> (Endpoint, std::sync::Arc<std::sync::Mutex<Vec<Seen>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Seen>::new()));
        let record = seen.clone();
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move |headers: axum::http::HeaderMap, body: String| {
                let responses = responses.clone();
                let record = record.clone();
                async move {
                    let beta = headers
                        .get("anthropic-beta")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    let body = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let i = {
                        let mut record = record.lock().expect("record lock");
                        record.push(Seen { beta, body });
                        record.len() - 1
                    };
                    let (status, body) = responses[i.min(responses.len() - 1)];
                    let content_type = if status == 200 { "text/event-stream" } else { "application/json" };
                    (
                        axum::http::StatusCode::from_u16(status).expect("valid status"),
                        [(axum::http::header::CONTENT_TYPE, content_type)],
                        body,
                    )
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        // A credential of its own: `RECOVERY` outlives the test, and a later
        // test can be handed the same port.
        static UPSTREAMS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = UPSTREAMS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let endpoint = Endpoint {
            base_url: format!("http://{addr}"),
            auth: Auth::ApiKey(format!("recording-upstream-{n}")),
        };
        (endpoint, seen)
    }

    /// A request replaying an earlier turn's thinking block.
    fn request_replaying_thinking(thinking: Option<super::super::types::ThinkingConfig>) -> CreateMessageRequest {
        CreateMessageRequest {
            model: "claude-opus-5-5".to_string(),
            max_tokens: 100,
            system: Some("prompt".to_string()),
            messages: vec![
                super::super::types::AnthropicMessage {
                    role: "user".to_string(),
                    content: vec![ContentBlock::Text { text: "hi".to_string() }],
                },
                super::super::types::AnthropicMessage {
                    role: "assistant".to_string(),
                    content: vec![
                        ContentBlock::Thinking { thinking: String::new(), signature: "sig".to_string() },
                        ContentBlock::Text { text: "hello".to_string() },
                    ],
                },
                super::super::types::AnthropicMessage {
                    role: "user".to_string(),
                    content: vec![ContentBlock::Text { text: "again".to_string() }],
                },
            ],
            stream: true,
            tools: vec![],
            thinking,
        }
    }

    fn requests_seen(record: &std::sync::Arc<std::sync::Mutex<Vec<Seen>>>) -> Vec<Seen> {
        record.lock().expect("record lock").clone()
    }

    #[test]
    fn test_dropped_thinking_counts_the_drops_message_start_reports() {
        let start = serde_json::json!({"type": "message_start", "message": {"id": "msg_1", "input_transformations": [
            {"type": "thinking_dropped", "path": "messages.1.content.0", "reason": "prefix_binding_mismatch"},
            {"type": "thinking_dropped", "path": "messages.3.content.0", "reason": "model_binding_mismatch"},
            {"type": "thinking_mismatch_allowed", "path": "messages.5.content.0", "reason": "prefix_binding_mismatch"},
            {"type": "some_future_entry"}
        ]}});
        assert_eq!(dropped_thinking(&start), 2);
        let empty = serde_json::json!({"type": "message_start", "message": {"input_transformations": []}});
        assert_eq!(dropped_thinking(&empty), 0);
        let absent = serde_json::json!({"type": "message_start", "message": {"id": "msg_1"}});
        assert_eq!(dropped_thinking(&absent), 0);
        let other = serde_json::json!({"type": "content_block_stop", "index": 0});
        assert_eq!(dropped_thinking(&other), 0);
    }

    #[tokio::test]
    async fn test_a_binding_mismatch_is_retried_with_drop_block() {
        let (endpoint, record) = recording_upstream(vec![(400, BINDING_BODY), (200, OK_BODY)]).await;
        let request = request_replaying_thinking(Some(super::super::types::ThinkingConfig::Adaptive));
        let result = stream_anthropic_message(&endpoint, &request, |_| {}).await;
        assert!(result.is_ok(), "should recover: {result:?}");
        let seen = requests_seen(&record);
        assert_eq!(seen.len(), 2);
        assert!(!seen[0].has_binding_beta() && seen[0].drop_block().is_none(), "first try is as before: {:?}", seen[0]);
        assert!(seen[1].has_binding_beta(), "the retry carries the beta: {:?}", seen[1].beta);
        assert_eq!(seen[1].drop_block(), Some("drop_block"));
        assert_eq!(seen[1].thinking_blocks(), 1, "drop_block replays the history verbatim");
    }

    #[tokio::test]
    async fn test_a_provider_that_needed_drop_block_gets_it_from_the_start() {
        let (endpoint, record) = recording_upstream(vec![(400, BINDING_BODY), (200, OK_BODY)]).await;
        let request = request_replaying_thinking(Some(super::super::types::ThinkingConfig::Adaptive));
        stream_anthropic_message(&endpoint, &request, |_| {}).await.expect("first turn recovers");
        stream_anthropic_message(&endpoint, &request, |_| {}).await.expect("second turn");
        let seen = requests_seen(&record);
        assert_eq!(seen.len(), 3, "the second turn needs no retry");
        assert!(seen[2].has_binding_beta());
        assert_eq!(seen[2].drop_block(), Some("drop_block"));

        let (other, other_record) = recording_upstream(vec![(200, OK_BODY)]).await;
        stream_anthropic_message(&other, &request, |_| {}).await.expect("another provider");
        let other_seen = requests_seen(&other_record);
        assert!(!other_seen[0].has_binding_beta() && other_seen[0].drop_block().is_none(), "another provider is untouched");
    }

    #[tokio::test]
    async fn test_a_binding_mismatch_without_thinking_is_retried_stripped() {
        let (endpoint, record) = recording_upstream(vec![(400, BINDING_BODY), (200, OK_BODY)]).await;
        let request = request_replaying_thinking(None);
        let result = stream_anthropic_message(&endpoint, &request, |_| {}).await;
        assert!(result.is_ok(), "should recover: {result:?}");
        let seen = requests_seen(&record);
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].thinking_blocks(), 0, "the retry drops every thinking block");
        assert!(!seen[1].has_binding_beta() && seen[1].drop_block().is_none());
        assert_eq!(seen[1].body["messages"][1]["content"][0]["text"], "hello", "the rest of the turn stays");
    }

    #[tokio::test]
    async fn test_a_strip_for_a_thinking_off_request_isnt_kept_for_thinking_on_ones() {
        let (endpoint, record) = recording_upstream(vec![
            (400, BINDING_BODY),
            (200, OK_BODY),
            (400, BINDING_BODY),
            (200, OK_BODY),
        ])
        .await;
        stream_anthropic_message(&endpoint, &request_replaying_thinking(None), |_| {})
            .await
            .expect("thinking off recovers by stripping");
        let thinking_on = request_replaying_thinking(Some(super::super::types::ThinkingConfig::Adaptive));
        stream_anthropic_message(&endpoint, &thinking_on, |_| {}).await.expect("thinking on recovers");
        let seen = requests_seen(&record);
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[2].thinking_blocks(), 1, "a thinking-on request goes out as built first");
        assert!(!seen[2].has_binding_beta());
        assert_eq!(seen[3].drop_block(), Some("drop_block"), "and is retried with drop_block, not stripped");
    }

    #[tokio::test]
    async fn test_a_refused_drop_block_falls_back_to_stripping() {
        let (endpoint, record) =
            recording_upstream(vec![(400, BINDING_BODY), (400, EXTRA_INPUTS_BODY), (200, OK_BODY)]).await;
        let request = request_replaying_thinking(Some(super::super::types::ThinkingConfig::Adaptive));
        let result = stream_anthropic_message(&endpoint, &request, |_| {}).await;
        assert!(result.is_ok(), "should recover by stripping: {result:?}");
        let seen = requests_seen(&record);
        assert_eq!(seen[2].thinking_blocks(), 0);
        assert!(!seen[2].has_binding_beta() && seen[2].drop_block().is_none());
        stream_anthropic_message(&endpoint, &request, |_| {}).await.expect("next turn");
        let seen = requests_seen(&record);
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[3].thinking_blocks(), 1, "the next turn isn't stripped ahead of a refusal");
    }

    #[tokio::test]
    async fn test_an_unrelated_400_under_drop_block_is_not_retried() {
        let too_long = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 1200000 tokens > 1000000 maximum"}}"#;
        let (endpoint, record) =
            recording_upstream(vec![(400, BINDING_BODY), (200, OK_BODY), (400, too_long), (200, OK_BODY)]).await;
        let request = request_replaying_thinking(Some(super::super::types::ThinkingConfig::Adaptive));
        stream_anthropic_message(&endpoint, &request, |_| {}).await.expect("learns drop_block");
        let error = stream_anthropic_message(&endpoint, &request, |_| {})
            .await
            .expect_err("an unrelated 400 is returned");
        assert!(error.contains("prompt is too long"), "the real cause is shown: {error}");
        assert_eq!(requests_seen(&record).len(), 3, "not retried stripped");
        stream_anthropic_message(&endpoint, &request, |_| {}).await.expect("next turn");
        let seen = requests_seen(&record);
        assert_eq!(seen[3].drop_block(), Some("drop_block"), "the provider still gets drop_block");
    }

    #[tokio::test]
    async fn test_a_binding_mismatch_on_the_last_retry_is_returned() {
        let (endpoint, record) = recording_upstream(vec![(400, BINDING_BODY)]).await;
        let request = request_replaying_thinking(None);
        let result = stream_anthropic_message(&endpoint, &request, |_| {}).await;
        let error = result.expect_err("a stripped history that still fails can't be fixed by retrying");
        assert!(error.contains("bound to a different conversation"), "{error}");
        assert_eq!(requests_seen(&record).len(), 2);
    }

    #[tokio::test]
    async fn test_a_persistent_provider_error_gives_up_readably() {
        let (result, requests) = run_against_responses(vec![(503, PAUSED_PROVIDER_BODY)]).await;
        assert_eq!(requests, 3, "three attempts, then give up");
        let error = result.expect_err("a provider that stays down should fail");
        assert!(error.contains("Provider 'featherless-ai' is currently failing"), "got {error}");
        assert!(!error.contains("{"), "raw JSON leaked into the message: {error}");
    }

    #[test]
    fn test_provider_error_message_unwraps_nested_json() {
        let message = provider_error_message(PAUSED_PROVIDER_BODY);
        assert_eq!(
            message,
            "Provider 'featherless-ai' is currently failing for model 'Qwen/Qwen3.8-27B' and has \
             been paused by the circuit breaker. Retry later or use another provider."
        );
    }

    #[test]
    fn test_provider_error_message_falls_back_to_the_raw_body() {
        assert_eq!(provider_error_message("upstream exploded"), "upstream exploded");
    }

    #[tokio::test]
    async fn test_stream_anthropic_message_assembles_turns_from_mock_upstream() {
        let text_mock_body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\"}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo!\"}}\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n",
            "\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n",
            "\n",
        );

        let mut deltas = Vec::new();
        let turn =
            run_against_mock_upstream(text_mock_body, |delta| deltas.push(delta.to_string()))
                .await
                .expect("stream should succeed");

        assert_eq!(deltas, vec!["Hel".to_string(), "lo!".to_string()]);
        assert_eq!(
            turn.content,
            vec![ContentBlock::Text {
                text: "Hello!".to_string()
            }]
        );
        assert_eq!(turn.stop_reason, "end_turn");

        let tool_use_mock_body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\"}\n",
            "\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"add\",\"input\":{}}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"2,\\\"b\\\":3}\"}}\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n",
            "\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n",
            "\n",
        );

        let turn = run_against_mock_upstream(tool_use_mock_body, |_| {
            panic!("no text deltas expected in a pure tool_use turn");
        })
        .await
        .expect("stream should succeed");

        assert_eq!(
            turn.content,
            vec![ContentBlock::ToolUse {
                id: "toolu_01".to_string(),
                name: "add".to_string(),
                input: serde_json::json!({"a": 2, "b": 3}),
            }]
        );
        assert_eq!(turn.stop_reason, "tool_use");

        // A thinking block, always first when present, followed by the
        // actual reply — `on_delta` should only ever see the text half.
        let thinking_mock_body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\"}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me \"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"think.\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig123\"}}\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"42\"}}\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n",
            "\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n",
            "\n",
        );

        let mut deltas = Vec::new();
        let turn =
            run_against_mock_upstream(thinking_mock_body, |delta| deltas.push(delta.to_string()))
                .await
                .expect("stream should succeed");

        assert_eq!(
            deltas,
            vec!["42".to_string()],
            "on_delta should only fire for the text block, never the thinking block"
        );
        assert_eq!(
            turn.content,
            vec![
                ContentBlock::Thinking {
                    thinking: "Let me think.".to_string(),
                    signature: "sig123".to_string(),
                },
                ContentBlock::Text {
                    text: "42".to_string()
                },
            ]
        );
        assert_eq!(turn.stop_reason, "end_turn");
    }

    #[tokio::test]
    async fn test_stream_anthropic_message_captures_real_usage_from_mock_upstream() {
        let mock_body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"usage\":{\"input_tokens\":2095,\"cache_creation_input_tokens\":10,\"cache_read_input_tokens\":5,\"output_tokens\":3}}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi!\"}}\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":45}}\n",
            "\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n",
            "\n",
        );

        let turn = run_against_mock_upstream(mock_body, |_| {})
            .await
            .expect("stream should succeed");

        // input_tokens/cache fields come from message_start; output_tokens
        // is message_delta's final value, not message_start's placeholder 3.
        assert_eq!(
            turn.usage,
            TokenUsage {
                input_tokens: 2095,
                output_tokens: 45,
                cache_creation_input_tokens: 10,
                cache_read_input_tokens: 5,
            }
        );
    }

    /// Regression test for SME-8's retrospective's hung
    /// live model call: before `RESPONSE_TIMEOUT` existed, a connection
    /// that never got a response at all (accepted, then silence) hung
    /// `stream_anthropic_message` forever. Accepts the connection but never
    /// writes a response, so `send_and_await_response` has nothing to read
    /// — a short `response_timeout` (not the real `RESPONSE_TIMEOUT`, which
    /// would make this test take 10 real minutes) must still make it
    /// return promptly rather than hang.
    #[tokio::test]
    async fn test_send_and_await_response_times_out_when_upstream_never_responds() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                // Hold the connection open without ever writing a response,
                // for comfortably longer than the test's own timeout below.
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                drop(stream);
            }
        });

        let request = CreateMessageRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 100,
            system: None,
            messages: vec![],
            stream: true,
            tools: vec![],
            thinking: None,
        };

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            send_and_await_response(
                &test_endpoint(addr),
                &request,
                Binding::AsIs,
                std::time::Duration::from_millis(50),
            ),
        )
        .await
        .expect("send_and_await_response itself should not hang past its own timeout");

        match result {
            Err(e) => assert_eq!(e, "timed out waiting for Anthropic to respond"),
            Ok(_) => panic!("expected a timeout error, got a response"),
        }
    }

    /// Spins up a throwaway mock upstream that captures the request's
    /// headers (instead of caring about the body) and calls
    /// `send_and_await_response` against it — for asserting exactly which
    /// auth header a given credential actually sends.
    async fn send_and_capture_headers(auth: Auth) -> (
        Result<reqwest::Response, String>,
        Option<axum::http::HeaderMap>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        let captured: std::sync::Arc<std::sync::Mutex<Option<axum::http::HeaderMap>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_for_route = captured.clone();
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let captured = captured_for_route.clone();
                async move {
                    *captured.lock().unwrap_or_else(|e| e.into_inner()) = Some(headers);
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        "",
                    )
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let request = CreateMessageRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 100,
            system: None,
            messages: vec![],
            stream: true,
            tools: vec![],
            thinking: None,
        };

        let endpoint = Endpoint {
            base_url: format!("http://{addr}"),
            auth,
        };
        let result =
            send_and_await_response(&endpoint, &request, Binding::AsIs, std::time::Duration::from_secs(5)).await;
        let headers = captured.lock().unwrap_or_else(|e| e.into_inner()).clone();
        (result, headers)
    }

    #[tokio::test]
    async fn test_send_and_await_response_uses_bearer_auth_for_a_bearer_provider() {
        let (result, headers) =
            send_and_capture_headers(Auth::Bearer("hf-token-value".to_string())).await;
        result.expect("request should succeed");
        let headers = headers.expect("mock upstream should have received the request");
        assert_eq!(
            headers.get("authorization").expect("Authorization header"),
            "Bearer hf-token-value"
        );
        assert!(
            headers.get("x-api-key").is_none(),
            "should not also send x-api-key for a bearer provider"
        );
    }

    #[tokio::test]
    async fn test_send_and_await_response_uses_x_api_key_for_an_api_key_provider() {
        let (result, headers) =
            send_and_capture_headers(Auth::ApiKey("api-key-value".to_string())).await;
        result.expect("request should succeed");
        let headers = headers.expect("mock upstream should have received the request");
        assert_eq!(
            headers.get("x-api-key").expect("x-api-key header"),
            "api-key-value"
        );
        assert!(
            headers.get("authorization").is_none(),
            "should not send Authorization for an api-key provider"
        );
    }

    /// SME-41 D11: a failed request names the model, not Claude, since
    /// the endpoint is often something else (a local llama.cpp here).
    #[tokio::test]
    async fn test_an_unreachable_model_endpoint_is_reported_as_the_model() {
        let result = send_and_await_response(
            &Endpoint {
                base_url: "http://127.0.0.1:1".to_string(),
                auth: Auth::ApiKey("key".to_string()),
            },
            &CreateMessageRequest {
                model: "local".to_string(),
                max_tokens: 100,
                system: None,
                messages: vec![],
                stream: true,
                tools: vec![],
                thinking: None,
            },
            Binding::AsIs,
            std::time::Duration::from_secs(5),
        )
        .await;
        let message = result.expect_err("nothing listens on port 1");
        assert!(message.starts_with("The model request failed"), "got: {message}");
    }
}
