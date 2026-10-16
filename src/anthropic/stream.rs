//! Streaming client for the real Anthropic Messages API (`stream: true`).
//! Distinct from and unrelated to Dioxus's own `ServerEvents` transport
//! between the browser and this server — this module only talks to Anthropic.

use futures_util::StreamExt;
use serde_json::Value;

use super::types::{ContentBlock, CreateMessageRequest, TokenUsage};

mod endpoint;
mod lines;

pub use endpoint::{Auth, Endpoint};

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
    /// The tool call a cut-off reply stopped in the middle of, by name
    /// (SME-126): dropped from `content`, never run.
    pub cut_off_call: Option<String>,
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

/// The `index` a `content_block_*` event names, the block it belongs to.
fn block_index(value: &Value) -> Option<usize> {
    value
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|i| usize::try_from(i).ok())
}

/// Accumulates one in-progress content block across its
/// `content_block_start` → `content_block_delta`* → `content_block_stop`
/// events. Several can be open at once: Anthropic and Ollama close each
/// block before starting the next, but llama.cpp opens thinking, text and
/// each tool call without closing any, and stops them all at the end of
/// the reply (SME-112). See `Blocks`.
#[derive(Debug)]
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

/// A finished block. A tool call whose input doesn't parse is kept until
/// the reply's `stop_reason` is known (SME-126): a reply cut off at its
/// limit stops every block before saying so, and the call it was cut in
/// is dropped rather than failing the reply. Any other is the error it
/// always was.
#[derive(Debug)]
enum Finished {
    Block(ContentBlock),
    UnparsedCall { name: String, error: String },
}

impl Finished {
    fn tool_name(&self) -> Option<&str> {
        match self {
            Finished::Block(ContentBlock::ToolUse { name, .. }) | Finished::UnparsedCall { name, .. } => Some(name),
            Finished::Block(_) => None,
        }
    }
}

impl PartialBlock {
    fn finalize(self) -> Finished {
        match self {
            PartialBlock::Text(text) => Finished::Block(ContentBlock::Text { text }),
            PartialBlock::ToolUse {
                id,
                name,
                partial_json,
            } => {
                if partial_json.is_empty() {
                    let input = Value::Object(serde_json::Map::new());
                    return Finished::Block(ContentBlock::ToolUse { id, name, input });
                }
                match serde_json::from_str(&partial_json) {
                    Ok(input) => Finished::Block(ContentBlock::ToolUse { id, name, input }),
                    Err(e) => Finished::UnparsedCall {
                        error: format!("failed to parse tool_use input JSON for {name}: {e}"),
                        name,
                    },
                }
            }
            PartialBlock::Thinking {
                thinking,
                signature,
            } => Finished::Block(ContentBlock::Thinking {
                thinking,
                signature,
            }),
        }
    }
}

/// A tool name as a provider accepts one (letters, digits, `_`, `-`;
/// `.` too, for MCP tools' own names), at most 128 characters.
fn is_plain_tool_name(name: &str) -> bool {
    (1..=128).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// Whether a reply with this `stop_reason` was cut off at its limit
/// rather than finished (SME-111): its budget, or the context window.
pub fn is_cut_off(stop_reason: &str) -> bool {
    matches!(stop_reason, "max_tokens" | "model_context_window_exceeded")
}

/// Whether a reply with this `stop_reason` stopped before it finished:
/// cut off (`is_cut_off`), or refused by the provider (B3, SME-122). Either
/// can stop it in the middle of a tool call, whose input may be cut short.
pub fn stopped_early(stop_reason: &str) -> bool {
    is_cut_off(stop_reason) || stop_reason == "refusal"
}

/// A reply's content blocks as they stream, keyed by each event's `index`
/// (SME-112). A block is opened by its start or its first delta, and
/// finished by its stop or the end of the stream; the reply's content is
/// in index order, whatever order the stops came in. An event without an
/// index (no real provider sends one) belongs to the block opened last,
/// or opens one after every block seen so far.
#[derive(Default)]
struct Blocks {
    open: std::collections::BTreeMap<usize, PartialBlock>,
    /// Finished blocks by (index, order finished): a block reopened at an
    /// index already finished lands right after the first.
    done: std::collections::BTreeMap<(usize, usize), Finished>,
    last: Option<usize>,
}

impl Blocks {
    fn open_at(&mut self, index: Option<usize>) -> Option<&mut PartialBlock> {
        self.open.get_mut(&index.or(self.last)?)
    }

    /// Opens `block` at `index` (or after every block seen, without one).
    /// A block still open there is finished first.
    fn start(&mut self, index: Option<usize>, block: PartialBlock) {
        let index = index.unwrap_or_else(|| {
            let seen = self.open.keys().chain(self.done.keys().map(|(i, _)| i));
            seen.max().map_or(0, |i| i.saturating_add(1))
        });
        // The same tool call started again while open (llama.cpp's final
        // chunk can resend a call's start): keep the one call and its input.
        if let (Some(PartialBlock::ToolUse { id, name, .. }), PartialBlock::ToolUse { id: new_id, name: new_name, .. }) =
            (self.open.get_mut(&index), &block)
            && id == new_id
        {
            name.clone_from(new_name);
            self.last = Some(index);
            return;
        }
        if self.open.contains_key(&index) || self.done.keys().any(|(i, _)| *i == index) {
            tracing::warn!(index, "the model provider reused a content block's index");
        }
        self.stop(Some(index));
        self.open.insert(index, block);
        self.last = Some(index);
    }

    /// Finishes the block at `index` (or the one opened last). A stop
    /// for a block that isn't open is ignored.
    fn stop(&mut self, index: Option<usize>) {
        let Some(index) = index.or(self.last) else {
            return;
        };
        if let Some(block) = self.open.remove(&index) {
            let order = self.done.len();
            self.done.insert((index, order), block.finalize());
        }
    }

    fn text(&mut self, index: Option<usize>, text: String) {
        if let Some(PartialBlock::Text(existing)) = self.open_at(index) {
            existing.push_str(&text);
            return;
        }
        self.start(index, PartialBlock::Text(text));
    }

    fn thinking(&mut self, index: Option<usize>, thinking: String) {
        if let Some(PartialBlock::Thinking { thinking: existing, .. }) = self.open_at(index) {
            existing.push_str(&thinking);
            return;
        }
        self.start(
            index,
            PartialBlock::Thinking {
                thinking,
                signature: String::new(),
            },
        );
    }

    /// Dropped unless its index holds an open thinking block.
    fn signature(&mut self, index: Option<usize>, signature: &str) {
        if let Some(PartialBlock::Thinking { signature: existing, .. }) = self.open_at(index) {
            existing.push_str(signature);
        }
    }

    /// Dropped unless its index holds an open tool call.
    fn tool_input(&mut self, index: Option<usize>, partial_json: &str) {
        if let Some(PartialBlock::ToolUse { partial_json: existing, .. }) = self.open_at(index) {
            existing.push_str(partial_json);
        }
    }

    /// Finishes every block still open and returns them all in index
    /// order. When the reply stopped early (`stopped_early`) and its last block
    /// is a tool call, that call is dropped, whether or not its input
    /// parsed, and its name returned (SME-126). A call left whose input
    /// doesn't parse fails the reply.
    fn finish(mut self, stop_reason: &str) -> Result<(Vec<ContentBlock>, Option<String>), String> {
        while let Some(&index) = self.open.keys().next() {
            self.stop(Some(index));
        }
        let mut cut_off_call = None;
        if stopped_early(stop_reason)
            && let Some(last) = self.done.last_entry()
            && let Some(name) = last.get().tool_name()
        {
            // The name goes into a notice the model reads as the user's
            // message: only a plain tool name is passed on. A call with no
            // name, or one that could carry words, is dropped all the same,
            // unnamed (review rounds 1 and 2).
            cut_off_call = Some(name.to_string()).filter(|name| is_plain_tool_name(name));
            last.remove();
        }
        let content = self
            .done
            .into_values()
            .map(|finished| match finished {
                Finished::Block(block) => Ok(block),
                Finished::UnparsedCall { error, .. } => Err(error),
            })
            .collect::<Result<_, _>>()?;
        Ok((content, cut_off_call))
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

fn forget_binding(endpoint: &Endpoint) {
    if let Ok(mut known) = RECOVERY.lock() {
        known.remove(&provider_key(endpoint));
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

/// Whether a 400's body refuses the controls beta: its `block_binding`
/// field, or the beta value in the `anthropic-beta` header.
fn refuses_binding_controls(body: &str) -> bool {
    body.contains("block_binding") || body.contains(BINDING_BETA)
}

/// The next form to try after `binding` was refused with a 400 whose body
/// is `body`, or `None` when another try won't help. `tried` lists the
/// forms already sent for this request; each is sent at most once.
fn next_binding(binding: Binding, tried: &[Binding], request: &CreateMessageRequest, body: &str) -> Option<Binding> {
    let untried = |b: Binding| (!tried.contains(&b)).then_some(b);
    match binding {
        Binding::AsIs if is_binding_mismatch(body) && request.thinking.is_some() => {
            untried(Binding::DropBlock).or_else(|| untried(Binding::Strip))
        }
        Binding::AsIs if is_binding_mismatch(body) => untried(Binding::Strip),
        // A route that doesn't know the controls beta refuses the field
        // ("block_binding: Extra inputs are not permitted") or the beta
        // value itself; one that doesn't run the check may take the
        // request as built. Any other 400 isn't about the replayed
        // thinking, and retrying would only hide the real error.
        Binding::DropBlock if is_binding_mismatch(body) || refuses_binding_controls(body) => {
            untried(Binding::AsIs).or_else(|| untried(Binding::Strip))
        }
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
            stripped.to_body()
        }
        _ => request.to_body(),
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
///
/// Runs in a CLIENT span, `chat {model}`, with the GenAI conventions' model,
/// usage and finish reason, the transient retries and the thinking binding
/// it ended on (SME-137). A failure or a cut-off reply marks it failed. Its
/// text and the request's are never recorded.
pub async fn stream_anthropic_message(
    endpoint: &Endpoint,
    request: &CreateMessageRequest,
    on_delta: impl FnMut(&str),
) -> Result<StreamedTurn, String> {
    use tracing::field::Empty;
    let span = tracing::info_span!(
        "chat",
        otel.name = %format_args!("chat {}", request.model),
        otel.kind = "client",
        gen_ai.operation.name = "chat",
        gen_ai.request.model = %request.model,
        server.address = Empty,
        gen_ai.usage.input_tokens = Empty,
        gen_ai.usage.output_tokens = Empty,
        gen_ai.usage.cache_creation.input_tokens = Empty,
        gen_ai.usage.cache_read.input_tokens = Empty,
        gen_ai.response.finish_reasons = Empty,
        smelt.retries = Empty,
        smelt.binding = Empty,
        otel.status_code = Empty,
        otel.status_description = Empty,
    );
    // The host only: a base URL's path or userinfo stays out.
    if let Some(host) = url::Url::parse(&endpoint.base_url).ok().as_ref().and_then(url::Url::host_str) {
        span.record("server.address", host);
    }
    crate::telemetry::in_span(span.clone(), async move {
        let mut tries = Tries { retries: 0, binding: binding_for(endpoint, request) };
        let result = stream_message(endpoint, request, on_delta, &mut tries).await;
        // Each once: a span keeps every value an attribute is given.
        span.record("smelt.retries", tries.retries as i64);
        span.record("smelt.binding", tracing::field::debug(tries.binding));
        record_outcome(&span, &result);
        result
    })
    .await
}

/// How a model call got its answer: the transient retries it took and the
/// thinking binding it ended on.
struct Tries {
    retries: usize,
    binding: Binding,
}

/// A model call's usage and finish reason on its span; a failure or a
/// cut-off reply marks it failed.
fn record_outcome(span: &tracing::Span, result: &Result<StreamedTurn, String>) {
    match result {
        Ok(turn) => {
            span.record("gen_ai.usage.input_tokens", turn.usage.input_tokens);
            span.record("gen_ai.usage.output_tokens", turn.usage.output_tokens);
            span.record("gen_ai.usage.cache_creation.input_tokens", turn.usage.cache_creation_input_tokens);
            span.record("gen_ai.usage.cache_read.input_tokens", turn.usage.cache_read_input_tokens);
            span.record("gen_ai.response.finish_reasons", turn.stop_reason.as_str());
            if is_cut_off(&turn.stop_reason) {
                crate::telemetry::mark_error(span, &format!("the reply was cut off ({})", turn.stop_reason));
            }
        }
        Err(e) => crate::telemetry::mark_error(span, e),
    }
}

/// `stream_anthropic_message`'s work, in its span.
async fn stream_message(
    endpoint: &Endpoint,
    request: &CreateMessageRequest,
    mut on_delta: impl FnMut(&str),
    tries: &mut Tries,
) -> Result<StreamedTurn, String> {
    // Held through the retries and the whole stream (SME-162).
    let _slot = endpoint.slot().await;
    // Retried only here, before anything has streamed: nothing has been
    // shown to the viewer yet, so a retry is invisible to them.
    let mut attempt = 0;
    let mut binding = tries.binding;
    let mut tried = vec![binding];
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
            tries.retries = attempt;
            continue;
        }
        let body = response.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::BAD_REQUEST
            && let Some(next) = next_binding(binding, &tried, request, &body)
        {
            if binding == Binding::DropBlock {
                forget_binding(endpoint);
            }
            tried.push(next);
            tracing::warn!(
                from = ?binding,
                to = ?next,
                error = %provider_error_message(&body),
                "model provider refused the replayed thinking; retrying"
            );
            binding = next;
            tries.binding = binding;
            continue;
        }
        let message = provider_error_message(&body);
        // A server that doesn't know prompt caching names the field it
        // refused; the fix is the provider's setting, not a retry. The same
        // goes for a model refusing effort, or the level asked for.
        let hint = if request.prompt_caching && message.contains("cache_control") {
            " (This server may not support prompt caching: turn off \u{201c}Prompt caching\u{201d} on its provider.)"
        } else if request.output_config.is_some()
            && (message.contains("effort") || message.contains("output_config"))
        {
            " (This model may not support this effort level: change or clear \u{201c}Effort\u{201d} for it on its provider's models.)"
        } else {
            ""
        };
        return Err(format!("model provider error {status}: {message}{hint}"));
    };

    let mut byte_stream = response.bytes_stream();
    let mut lines = lines::LineBuffer::default();
    let mut blocks = Blocks::default();
    let mut stop_reason = String::new();
    let mut usage = TokenUsage::default();

    loop {
        let next = tokio::time::timeout(CHUNK_TIMEOUT, byte_stream.next())
            .await
            .map_err(|_| "timed out waiting for the next chunk from Anthropic".to_string())?;

        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|e| format!("error reading Anthropic response stream: {e}"))?;
        lines.push(&chunk);

        while let Some(line) = lines.next_line() {
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
            let index = block_index(&value);
            match interpret_stream_event(&value) {
                StreamOutcome::TextDelta(text) => {
                    on_delta(&text);
                    blocks.text(index, text);
                }
                // Not passed to `on_delta` — thinking is never part of the
                // live-typed reply, only shown (collapsed) once the full
                // block lands with the finished message. No live "typing"
                // effect for it, unlike text.
                StreamOutcome::ThinkingDelta(thinking) => blocks.thinking(index, thinking),
                StreamOutcome::ThinkingSignatureDelta(signature) => blocks.signature(index, &signature),
                StreamOutcome::ToolUseStart { id, name } => blocks.start(
                    index,
                    PartialBlock::ToolUse {
                        id,
                        name,
                        partial_json: String::new(),
                    },
                ),
                StreamOutcome::ToolUseInputDelta(partial_json) => blocks.tool_input(index, &partial_json),
                StreamOutcome::BlockStop => blocks.stop(index),
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

    let (content, cut_off_call) = blocks.finish(&stop_reason)?;
    Ok(StreamedTurn {
        content,
        stop_reason,
        usage,
        cut_off_call,
    })
}

#[cfg(test)]
mod tests;
