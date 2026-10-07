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
    ModelDetails {
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
mod tests {
    use super::*;
    use crate::anthropic::stream::Auth;

    fn json(text: &str) -> Value {
        serde_json::from_str(text).expect("a fixture is JSON")
    }

    const ANTHROPIC: &str = include_str!("fixtures/anthropic_v1_models.json");
    const LLAMA_CPP: &str = include_str!("fixtures/llama_cpp_v1_models.json");
    const LLAMA_CPP_PROPS: &str = include_str!("fixtures/llama_cpp_props.json");
    const OLLAMA_TAGS: &str = include_str!("fixtures/ollama_api_tags.json");
    const OLLAMA_SHOW: &str = include_str!("fixtures/ollama_api_show.json");
    const OLLAMA_PS: &str = include_str!("fixtures/ollama_api_ps.json");

    #[test]
    fn test_anthropics_listing_gives_names_thinking_and_the_next_page() {
        let (models, next) = parse_v1_models(&json(ANTHROPIC)).expect("parses");
        assert_eq!(
            models,
            vec![ListedModel {
                id: "claude-opus-5".to_string(),
                display_name: Some("Claude Opus 5".to_string()),
                // The docs' example says 0 tokens: a placeholder, not a size.
                details: ModelDetails { context_window: None, thinking: Some(true), tools: None, max_output: None },
            }]
        );
        assert_eq!(next.as_deref(), Some("last_id"), "has_more, so continue after last_id");
    }

    #[test]
    fn test_a_real_llama_cpp_listing_gives_its_runtime_context_window() {
        let (models, next) = parse_v1_models(&json(LLAMA_CPP)).expect("parses");
        assert_eq!(
            models,
            vec![ListedModel {
                id: "flash-next".to_string(),
                display_name: None,
                details: ModelDetails { context_window: Some(262_144), thinking: None, tools: None, max_output: None },
            }]
        );
        assert_eq!(next, None, "no has_more");
    }

    /// SME-111: the user's real llama-server's `/props`.
    #[test]
    fn test_a_real_llama_cpp_props_gives_its_window_slots_and_template_caps() {
        let info = parse_llama_props(&json(LLAMA_CPP_PROPS));
        assert_eq!(info.n_ctx, Some(262_144), "default_generation_settings.n_ctx");
        assert_eq!(info.total_slots, Some(1));
        assert_eq!(info.model_alias.as_deref(), Some("flash-next"));
        assert_eq!(info.build_info.as_deref(), Some("b11434-5e03bdd87"));
        assert!(info.supports_reasoning_effort());
        assert!(info.supports_preserve_reasoning());
        assert_eq!(info.template_caps.get("supports_tool_calls"), Some(&true));
        assert_eq!(info.template_caps.len(), 9, "every cap it reports: {:?}", info.template_caps);
    }

    #[test]
    fn test_props_without_the_fields_says_nothing() {
        let info = parse_llama_props(&json(r#"{"default_generation_settings":{"n_ctx":0},"chat_template_caps":{"supports_tools":"yes"}}"#));
        assert_eq!(info, LlamaServerInfo::default(), "a zero window and a non-boolean cap aren't anything");
        assert!(!info.supports_reasoning_effort());
    }

    #[test]
    fn test_only_llama_cpps_props_look_like_llama_cpp() {
        assert!(looks_like_llama_cpp(&json(LLAMA_CPP_PROPS)));
        assert!(!looks_like_llama_cpp(&json(OLLAMA_SHOW)), "Ollama's /api/show");
        assert!(!looks_like_llama_cpp(&json(ANTHROPIC)));
        assert!(!looks_like_llama_cpp(&json(r#"{}"#)));
        assert!(!looks_like_llama_cpp(&json(r#"[1, 2]"#)));
    }

    fn listed(id: &str, window: Option<u32>) -> ListedModel {
        ListedModel {
            id: id.to_string(),
            display_name: None,
            details: ModelDetails { context_window: window, thinking: None, tools: None, max_output: None },
        }
    }

    #[test]
    fn test_props_apply_to_the_model_the_server_serves() {
        let info = parse_llama_props(&json(LLAMA_CPP_PROPS));
        let mut one = vec![listed("anything", Some(4096))];
        apply_llama_props(&mut one, &info);
        assert_eq!(one[0].details, ModelDetails { context_window: Some(262_144), thinking: None, tools: Some(true), max_output: None }, "the only one listed");

        let mut several = vec![listed("other", Some(4096)), listed("flash-next", None)];
        apply_llama_props(&mut several, &info);
        assert_eq!(several[0].details.context_window, Some(4096), "not the alias: left alone");
        assert_eq!(several[0].details.tools, None);
        assert_eq!(several[1].details.context_window, Some(262_144), "the alias");

        let mut router = vec![listed("a", Some(8192)), listed("b", None)];
        apply_llama_props(&mut router, &info);
        assert_eq!(router, vec![listed("a", Some(8192)), listed("b", None)], "no model is the alias");
    }

    #[test]
    fn test_a_listing_without_data_is_an_error() {
        let error = parse_v1_models(&json(r#"{"error": "nope"}"#)).expect_err("no data array");
        assert!(error.contains("data"), "says what's missing: {error}");
    }

    /// SME-111: Anthropic's listing gives each model's output cap as
    /// `max_tokens`; the docs' example's 0 is a placeholder.
    #[test]
    fn test_an_anthropic_models_output_cap_is_its_max_tokens() {
        let body = json(r#"{"data":[{"id":"claude-x","max_input_tokens":1000000,"max_tokens":128000}],"has_more":false}"#);
        let (models, _) = parse_v1_models(&body).expect("parses");
        assert_eq!(models[0].details.max_output, Some(128_000));
        let (fixture, _) = parse_v1_models(&json(ANTHROPIC)).expect("parses");
        assert_eq!(fixture[0].details.max_output, None, "0 isn't a cap");
    }

    #[test]
    fn test_a_real_sized_anthropic_model_keeps_its_window() {
        let body = json(r#"{"data":[{"id":"claude-x","max_input_tokens":1000000,"capabilities":{"thinking":{"supported":false}}}],"has_more":false}"#);
        let (models, _) = parse_v1_models(&body).expect("parses");
        assert_eq!(
            models[0].details,
            ModelDetails { context_window: Some(1_000_000), thinking: Some(false), tools: None, max_output: None }
        );
    }

    #[test]
    fn test_ollamas_tags_give_model_names() {
        let models = parse_ollama_tags(&json(OLLAMA_TAGS)).expect("parses");
        assert_eq!(
            models,
            vec![ListedModel {
                id: "gemma4".to_string(),
                display_name: None,
                details: ModelDetails::default(),
            }]
        );
    }

    #[test]
    fn test_ollamas_show_gives_num_ctx_thinking_and_no_tools() {
        assert_eq!(
            parse_ollama_show(&json(OLLAMA_SHOW)),
            // The fixture's capabilities are completion, thinking and
            // vision: no tools. Its trained length (131072) isn't used.
            ModelDetails { context_window: Some(2048), thinking: Some(true), tools: Some(false), max_output: None }
        );
    }

    #[test]
    fn test_ollamas_show_without_num_ctx_or_capabilities_says_nothing() {
        let body = json(r#"{"parameters":"temperature 0.7","model_info":{"llama.context_length":8192}}"#);
        assert_eq!(parse_ollama_show(&body), ModelDetails::default());
    }

    #[test]
    fn test_ollamas_show_thinking_values_false_means_no_thinking() {
        let body = json(r#"{"capabilities":["completion","tools","thinking"],"thinking":{"values":[false],"default":false}}"#);
        assert_eq!(
            parse_ollama_show(&body),
            ModelDetails { context_window: None, thinking: Some(false), tools: Some(true), max_output: None }
        );
    }

    #[test]
    fn test_ollamas_ps_gives_a_loaded_models_window_only() {
        assert_eq!(parse_ollama_ps(&json(OLLAMA_PS), "gemma4"), Some(4096));
        assert_eq!(parse_ollama_ps(&json(OLLAMA_PS), "llama3"), None, "not loaded");
    }

    /// A mock provider serving `routes` (path, JSON body) and recording
    /// each request's path and query and auth headers.
    async fn mock_provider(
        routes: Vec<(&'static str, String)>,
    ) -> (Endpoint, std::sync::Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let app = axum::Router::new().fallback(move |request: axum::extract::Request| {
            let recorded = recorded.clone();
            let routes = routes.clone();
            async move {
                let uri = request.uri().clone();
                let key = request
                    .headers()
                    .get("x-api-key")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                recorded
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((uri.to_string(), key));
                match routes.iter().find(|(path, _)| *path == uri.path()) {
                    Some((_, body)) => (
                        axum::http::StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        body.clone(),
                    ),
                    None => (
                        axum::http::StatusCode::NOT_FOUND,
                        [(axum::http::header::CONTENT_TYPE, "text/plain")],
                        "404 page not found".to_string(),
                    ),
                }
            }
        });
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let endpoint = Endpoint {
            base_url: format!("http://{addr}/"),
            auth: Auth::ApiKey("key-1".to_string()),
        };
        (endpoint, seen)
    }

    #[tokio::test]
    async fn test_an_anthropic_listing_follows_its_pages() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let app = axum::Router::new().route(
            "/v1/models",
            axum::routing::get(|query: axum::extract::Query<std::collections::HashMap<String, String>>| async move {
                let body = match query.get("after_id").map(String::as_str) {
                    None => r#"{"data":[{"id":"a"}],"has_more":true,"last_id":"a"}"#,
                    Some("a") => r#"{"data":[{"id":"b"}],"has_more":false,"last_id":"b"}"#,
                    Some(_) => r#"{"data":[],"has_more":false}"#,
                };
                ([(axum::http::header::CONTENT_TYPE, "application/json")], body)
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let endpoint = Endpoint {
            base_url: format!("http://{addr}"),
            auth: Auth::ApiKey("k".to_string()),
        };
        let models = list_models(ProviderKind::Anthropic, &endpoint).await.expect("lists");
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn test_an_endless_anthropic_listing_stops() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let app = axum::Router::new().route(
            "/v1/models",
            axum::routing::get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    r#"{"data":[{"id":"same"}],"has_more":true,"last_id":"same"}"#,
                )
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let endpoint = Endpoint {
            base_url: format!("http://{addr}"),
            auth: Auth::ApiKey("k".to_string()),
        };
        let models = list_models(ProviderKind::Anthropic, &endpoint).await.expect("lists");
        assert_eq!(models.len(), MAX_PAGES, "one model per page, then it stops");
    }

    #[tokio::test]
    async fn test_ollama_is_listed_from_api_tags_with_its_key() {
        let (endpoint, seen) = mock_provider(vec![("/api/tags", OLLAMA_TAGS.to_string())]).await;
        let models = list_models(ProviderKind::Ollama, &endpoint).await.expect("lists");
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["gemma4"]);
        let seen = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(seen, vec![("/api/tags".to_string(), Some("key-1".to_string()))]);
    }

    #[tokio::test]
    async fn test_an_ollama_models_details_fall_back_to_ps_without_num_ctx() {
        let show = r#"{"parameters":"temperature 0.7","capabilities":["completion","tools"]}"#;
        let (endpoint, _) = mock_provider(vec![
            ("/api/show", show.to_string()),
            ("/api/ps", OLLAMA_PS.to_string()),
        ])
        .await;
        let details = model_details(ProviderKind::Ollama, &endpoint, "gemma4").await.expect("details");
        assert_eq!(
            details,
            ModelDetails { context_window: Some(4096), thinking: Some(false), tools: Some(true), max_output: None }
        );
    }

    #[tokio::test]
    async fn test_an_ollama_models_num_ctx_wins_without_asking_ps() {
        let (endpoint, seen) = mock_provider(vec![("/api/show", OLLAMA_SHOW.to_string())]).await;
        let details = model_details(ProviderKind::Ollama, &endpoint, "gemma4").await.expect("details");
        assert_eq!(details.context_window, Some(2048));
        let paths: Vec<String> = seen.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(paths, vec!["/api/show".to_string()]);
    }

    /// SME-72 review 3: one Anthropic model's details are one request.
    #[tokio::test]
    async fn test_an_anthropic_models_details_are_one_request() {
        let one = r#"{"id":"claude-x","display_name":"X","max_input_tokens":1000000,"capabilities":{"thinking":{"supported":true}},"type":"model"}"#;
        let (endpoint, seen) = mock_provider(vec![("/v1/models/claude-x", one.to_string())]).await;
        let details = model_details(ProviderKind::Anthropic, &endpoint, "claude-x").await.expect("details");
        assert_eq!(details, ModelDetails { context_window: Some(1_000_000), thinking: Some(true), tools: None, max_output: None });
        let paths: Vec<String> = seen.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(paths, vec!["/v1/models/claude-x".to_string()]);
    }

    #[tokio::test]
    async fn test_another_servers_model_details_come_from_its_listing() {
        let (endpoint, _) = mock_provider(vec![("/v1/models", LLAMA_CPP.to_string())]).await;
        let details = model_details(ProviderKind::Other, &endpoint, "flash-next").await.expect("details");
        assert_eq!(details.context_window, Some(262_144));
        let missing = model_details(ProviderKind::Other, &endpoint, "unlisted").await.expect("details");
        assert_eq!(missing, ModelDetails::default(), "a model the listing doesn't have: nothing known");
    }

    #[tokio::test]
    async fn test_a_provider_without_a_listing_says_why() {
        let (endpoint, _) = mock_provider(vec![]).await;
        let error = list_models(ProviderKind::Other, &endpoint).await.expect_err("404");
        assert!(error.contains("404"), "names the status: {error}");
    }

    #[tokio::test]
    async fn test_an_unreachable_provider_is_an_error_not_a_hang() {
        let endpoint = Endpoint {
            base_url: "http://127.0.0.1:1".to_string(),
            auth: Auth::ApiKey("k".to_string()),
        };
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            list_models(ProviderKind::Anthropic, &endpoint),
        )
        .await
        .expect("bounded")
        .expect_err("nothing listens on port 1");
        assert!(!error.is_empty());
    }
}
