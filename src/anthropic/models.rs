//! Listing a provider's models and what each one can do (SME-72). Turns go
//! to every provider's `/v1/messages`; how its models are listed depends
//! on its kind:
//!
//! - `anthropic`: `GET /v1/models`, paged, whose entries carry the context
//!   window (`max_input_tokens`) and thinking support.
//! - `ollama`: `GET /api/tags` for the names; `POST /api/show` per model
//!   for its tools and thinking support and a Modelfile's `num_ctx`, then
//!   `GET /api/ps` for the window it's loaded with if `num_ctx` isn't set.
//! - `llama_cpp`: `GET /v1/models` as for `other`, and `GET /props` for
//!   each slot's window, the slot count and the chat template's
//!   capabilities (SME-111).
//! - `other`: `GET /v1/models`, reading `max_input_tokens`, or llama.cpp's
//!   `meta.n_ctx`, when present.
//!
//! Every request is bounded by `REQUEST_TIMEOUT`; nothing here retries or
//! caches.

use serde_json::Value;

use super::stream::Endpoint;
use crate::providers::{LlamaServerInfo, ProviderKind};

/// The bound on each request here: the same as an MCP server's status
/// check, the other "is this service there" question the UI asks.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Anthropic's page size limit.
const PAGE_LIMIT: u32 = 1000;

/// How many pages of an Anthropic listing to follow.
const MAX_PAGES: usize = 10;

/// What a provider says about one model. `None` means it didn't say.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelDetails {
    pub context_window: Option<u32>,
    pub thinking: Option<bool>,
    pub tools: Option<bool>,
    /// The most a reply may be: Anthropic's `max_tokens` (SME-111).
    pub max_output: Option<u32>,
    /// Whether the model takes adaptive thinking, as Anthropic's listing
    /// says (`capabilities.thinking.types.adaptive`). Haiku 4.5 and older
    /// models don't, and refuse it: they take a budget (B9, SME-122).
    pub adaptive_thinking: Option<bool>,
}

/// One model in a provider's listing, with whatever details the listing
/// itself carries.
#[derive(Clone, Debug, PartialEq)]
pub struct ListedModel {
    pub id: String,
    pub display_name: Option<String>,
    pub details: ModelDetails,
}

/// Parses one page of a `/v1/models` listing (Anthropic's, or an
/// Anthropic- or OpenAI-shaped one from another server), returning its
/// models and the id to continue after when `has_more` says there's more.
fn parse_v1_models(body: &Value) -> Result<(Vec<ListedModel>, Option<String>), String> {
    let data = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or("the model listing has no \"data\" array")?;
    let models = data.iter().filter_map(parse_v1_model).collect();
    let next = match body.get("has_more").and_then(Value::as_bool) {
        Some(true) => body.get("last_id").and_then(Value::as_str).map(str::to_string),
        _ => None,
    };
    Ok((models, next))
}

/// One model entry of a `/v1/models` listing, or Anthropic's
/// `/v1/models/{id}`.
fn parse_v1_model(entry: &Value) -> Option<ListedModel> {
    let id = entry.get("id")?.as_str()?.to_string();
    // Anthropic's `max_input_tokens`, or llama.cpp's runtime
    // `meta.n_ctx`. Zero (the docs' placeholder) isn't a size.
    let size = |value: Option<&Value>| {
        value
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0)
    };
    let context_window = size(entry.get("max_input_tokens").or_else(|| entry.pointer("/meta/n_ctx")));
    Some(ListedModel {
        display_name: entry
            .get("display_name")
            .and_then(Value::as_str)
            .map(str::to_string),
        details: ModelDetails {
            adaptive_thinking: entry
                .pointer("/capabilities/thinking/types/adaptive/supported")
                .and_then(Value::as_bool),
            context_window,
            thinking: entry
                .pointer("/capabilities/thinking/supported")
                .and_then(Value::as_bool),
            tools: None,
            // Anthropic's output cap (SME-111).
            max_output: size(entry.get("max_tokens")),
        },
        id,
    })
}

/// Parses Ollama's `/api/tags`: the local models' names.
fn parse_ollama_tags(body: &Value) -> Result<Vec<ListedModel>, String> {
    let models = body
        .get("models")
        .and_then(Value::as_array)
        .ok_or("Ollama's model list has no \"models\" array")?;
    Ok(models
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name").or_else(|| entry.get("model"))?.as_str()?;
            Some(ListedModel {
                id: name.to_string(),
                display_name: None,
                details: ModelDetails::default(),
            })
        })
        .collect())
}

/// Parses Ollama's `/api/show` for one model: tools and thinking support,
/// and a Modelfile's `num_ctx`.
fn parse_ollama_show(body: &Value) -> ModelDetails {
    let capabilities: Option<Vec<&str>> = body
        .get("capabilities")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_str).collect());
    // `thinking.values` of just `[false]`: the model can't think.
    let thinking_values_off = body
        .pointer("/thinking/values")
        .and_then(Value::as_array)
        .is_some_and(|values| !values.is_empty() && values.iter().all(|v| v == &Value::Bool(false)));
    let thinking = capabilities
        .as_ref()
        .map(|caps| caps.contains(&"thinking") && !thinking_values_off);
    let context_window = body
        .get("parameters")
        .and_then(Value::as_str)
        .and_then(|parameters| {
            parameters.lines().find_map(|line| {
                let mut words = line.split_whitespace();
                (words.next() == Some("num_ctx")).then(|| words.next())?
            })
        })
        .and_then(|n| n.parse::<u32>().ok())
        .filter(|n| *n > 0);
    ModelDetails { adaptive_thinking: None,
        context_window,
        thinking,
        tools: capabilities.as_ref().map(|caps| caps.contains(&"tools")),
        max_output: None,
    }
}

/// The context window `model` is loaded with, from Ollama's `/api/ps`, if
/// it's loaded.
fn parse_ollama_ps(body: &Value, model: &str) -> Option<u32> {
    body.get("models")?
        .as_array()?
        .iter()
        .find(|entry| {
            ["name", "model"]
                .iter()
                .any(|key| entry.get(key).and_then(Value::as_str) == Some(model))
        })?
        .get("context_length")?
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
}

/// Parses a llama.cpp server's `/props`.
fn parse_llama_props(body: &Value) -> LlamaServerInfo {
    let count = |value: Option<&Value>| {
        value
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0)
    };
    let text = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_string);
    LlamaServerInfo {
        n_ctx: count(body.pointer("/default_generation_settings/n_ctx")),
        total_slots: count(body.get("total_slots")),
        model_alias: text("model_alias"),
        build_info: text("build_info"),
        template_caps: body
            .get("chat_template_caps")
            .and_then(Value::as_object)
            .map(|caps| {
                caps.iter()
                    .filter_map(|(name, value)| Some((name.clone(), value.as_bool()?)))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Whether a `/props` answer is llama.cpp's: the new-provider form then
/// suggests the kind (SME-111).
pub fn looks_like_llama_cpp(body: &Value) -> bool {
    body.get("chat_template_caps").is_some_and(Value::is_object)
        || body.pointer("/default_generation_settings/n_ctx").is_some()
}

/// Applies a llama.cpp server's `/props` to its listing: the slot's window
/// and the template's tool support go to the model it serves (its alias,
/// or the only one listed). A server in router mode lists several, and its
/// `/props` isn't any one of theirs.
pub fn apply_llama_props(listed: &mut [ListedModel], info: &LlamaServerInfo) {
    let only_one = listed.len() == 1;
    let tools = info.template_caps.get("supports_tool_calls").copied();
    for model in listed
        .iter_mut()
        .filter(|m| only_one || info.model_alias.as_deref() == Some(m.id.as_str()))
    {
        model.details.context_window = info.n_ctx.or(model.details.context_window);
        model.details.tools = tools.or(model.details.tools);
    }
}

/// `text` as one URL path segment: a model id may hold `/` or `:`.
fn url_segment(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Sends `request` (with `endpoint`'s credential) and reads a JSON body,
/// bounded by `REQUEST_TIMEOUT`. A non-success status is an error naming it.
async fn fetch_json(endpoint: &Endpoint, request: reqwest::RequestBuilder) -> Result<Value, String> {
    let send = endpoint.authorize(request).send();
    let response = tokio::time::timeout(REQUEST_TIMEOUT, send)
        .await
        .map_err(|_| "the provider didn't answer within 10 seconds".to_string())?
        .map_err(|e| format!("couldn't reach the provider: {e}"))?;
    let status = response.status();
    let body = tokio::time::timeout(REQUEST_TIMEOUT, response.text())
        .await
        .map_err(|_| "the provider's answer didn't finish within 10 seconds".to_string())?
        .map_err(|e| format!("couldn't read the provider's answer: {e}"))?;
    if !status.is_success() {
        let detail: String = body.trim().chars().take(200).collect();
        return Err(format!("the provider answered {status}: {detail}"));
    }
    serde_json::from_str(&body).map_err(|e| format!("the provider's answer isn't JSON: {e}"))
}

/// Every page of a `/v1/models` listing, up to `MAX_PAGES`.
async fn list_v1_models(endpoint: &Endpoint) -> Result<Vec<ListedModel>, String> {
    let client = endpoint.client()?;
    let mut models = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut request = client
            .get(endpoint.url("/v1/models"))
            .query(&[("limit", PAGE_LIMIT.to_string())]);
        if let Some(after) = &after {
            request = request.query(&[("after_id", after)]);
        }
        let (page, next) = parse_v1_models(&fetch_json(endpoint, request).await?)?;
        models.extend(page);
        match next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    Ok(models)
}

/// Lists `endpoint`'s models the way its `kind` does.
pub async fn list_models(kind: ProviderKind, endpoint: &Endpoint) -> Result<Vec<ListedModel>, String> {
    match kind {
        ProviderKind::Anthropic | ProviderKind::LlamaCpp | ProviderKind::Other => list_v1_models(endpoint).await,
        ProviderKind::Ollama => {
            let request = endpoint.client()?.get(endpoint.url("/api/tags"));
            parse_ollama_tags(&fetch_json(endpoint, request).await?)
        }
    }
}

/// A llama.cpp server's `/props`.
pub async fn llama_cpp_props(endpoint: &Endpoint) -> Result<LlamaServerInfo, String> {
    let request = endpoint.client()?.get(endpoint.url("/props"));
    Ok(parse_llama_props(&fetch_json(endpoint, request).await?))
}

/// Whether `base_url` answers `/props` as llama.cpp does, asked without
/// any key: a key entered for one address never goes to another (SME-111).
pub async fn probe_llama_cpp(base_url: &str) -> bool {
    let endpoint = Endpoint {
        base_url: base_url.trim().to_string(),
        auth: super::stream::Auth::ApiKey(String::new()),
        slots_of: None,
    };
    let Ok(client) = endpoint.client() else {
        return false;
    };
    let request = client.get(endpoint.url("/props"));
    let send = request.send();
    let Ok(Ok(response)) = tokio::time::timeout(REQUEST_TIMEOUT, send).await else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }
    match tokio::time::timeout(REQUEST_TIMEOUT, response.json::<Value>()).await {
        Ok(Ok(body)) => looks_like_llama_cpp(&body),
        _ => false,
    }
}

/// An Ollama server's details for each of `models`, `at_once` at a time:
/// `/api/show` each, and `/api/ps` once (it lists every loaded model) if
/// any of them has no `num_ctx`.
pub async fn ollama_details(
    endpoint: &Endpoint,
    models: Vec<String>,
    at_once: usize,
) -> Vec<(String, Result<ModelDetails, String>)> {
    use futures_util::StreamExt;

    let client = match endpoint.client() {
        Ok(client) => client,
        Err(e) => return models.into_iter().map(|m| (m, Err(e.clone()))).collect(),
    };
    let client = &client;
    let mut shown: Vec<(String, Result<ModelDetails, String>)> = futures_util::stream::iter(models)
        .map(|model| async move {
            let show = client
                .post(endpoint.url("/api/show"))
                .json(&serde_json::json!({ "model": model }));
            let details = fetch_json(endpoint, show).await.map(|body| parse_ollama_show(&body));
            (model, details)
        })
        .buffer_unordered(at_once.max(1))
        .collect()
        .await;
    let unsized_model = shown
        .iter()
        .any(|(_, details)| matches!(details, Ok(d) if d.context_window.is_none()));
    if unsized_model {
        match fetch_json(endpoint, client.get(endpoint.url("/api/ps"))).await {
            Ok(ps) => {
                for (model, details) in &mut shown {
                    if let Ok(d) = details
                        && d.context_window.is_none()
                    {
                        d.context_window = parse_ollama_ps(&ps, model);
                    }
                }
            }
            Err(e) => tracing::info!(error = %e, "couldn't ask Ollama what's loaded"),
        }
    }
    shown
}

/// What `endpoint` says about `model`. For an Ollama server that's
/// `/api/show` and maybe `/api/ps`; for the others, the model's entry in
/// the listing (nothing, if it isn't listed).
pub async fn model_details(
    kind: ProviderKind,
    endpoint: &Endpoint,
    model: &str,
) -> Result<ModelDetails, String> {
    match kind {
        // One request: Anthropic serves a single model's entry.
        ProviderKind::Anthropic => {
            let request = endpoint
                .client()?
                .get(endpoint.url(&format!("/v1/models/{}", url_segment(model))));
            let body = fetch_json(endpoint, request).await?;
            Ok(parse_v1_model(&body).map(|m| m.details).unwrap_or_default())
        }
        ProviderKind::LlamaCpp | ProviderKind::Other => Ok(list_v1_models(endpoint)
            .await?
            .into_iter()
            .find(|listed| listed.id == model)
            .map(|listed| listed.details)
            .unwrap_or_default()),
        ProviderKind::Ollama => {
            let client = endpoint.client()?;
            let show = client
                .post(endpoint.url("/api/show"))
                .json(&serde_json::json!({ "model": model }));
            let mut details = parse_ollama_show(&fetch_json(endpoint, show).await?);
            if details.context_window.is_none() {
                // Only while the model is loaded; not having it isn't an
                // error, just nothing more known.
                match fetch_json(endpoint, client.get(endpoint.url("/api/ps"))).await {
                    Ok(ps) => details.context_window = parse_ollama_ps(&ps, model),
                    Err(e) => tracing::info!(model, error = %e, "couldn't ask Ollama what's loaded"),
                }
            }
            Ok(details)
        }
    }
}

#[cfg(test)]
mod tests;
