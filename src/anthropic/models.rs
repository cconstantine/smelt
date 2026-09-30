//! Listing a provider's models and what each one can do (SME-72). Turns go
//! to every provider's `/v1/messages`; how its models are listed depends
//! on its kind:
//!
//! - `anthropic`: `GET /v1/models`, paged, whose entries carry the context
//!   window (`max_input_tokens`) and thinking support.
//! - `ollama`: `GET /api/tags` for the names; `POST /api/show` per model
//!   for its tools and thinking support and a Modelfile's `num_ctx`, then
//!   `GET /api/ps` for the window it's loaded with if `num_ctx` isn't set.
//! - `other`: `GET /v1/models`, reading `max_input_tokens`, or llama.cpp's
//!   `meta.n_ctx`, when present.
//!
//! Every request is bounded by `REQUEST_TIMEOUT`; nothing here retries or
//! caches.

use serde_json::Value;

use super::stream::Endpoint;
use crate::providers::ProviderKind;

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
    let models = data
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_string();
            // Anthropic's `max_input_tokens`, or llama.cpp's runtime
            // `meta.n_ctx`. Zero (the docs' placeholder) isn't a size.
            let context_window = entry
                .get("max_input_tokens")
                .or_else(|| entry.pointer("/meta/n_ctx"))
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n > 0);
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
                },
                id,
            })
        })
        .collect();
    let next = match body.get("has_more").and_then(Value::as_bool) {
        Some(true) => body.get("last_id").and_then(Value::as_str).map(str::to_string),
        _ => None,
    };
    Ok((models, next))
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
    let client = reqwest::Client::new();
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
        ProviderKind::Anthropic | ProviderKind::Other => list_v1_models(endpoint).await,
        ProviderKind::Ollama => {
            let request = reqwest::Client::new().get(endpoint.url("/api/tags"));
            parse_ollama_tags(&fetch_json(endpoint, request).await?)
        }
    }
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
        ProviderKind::Anthropic | ProviderKind::Other => Ok(list_v1_models(endpoint)
            .await?
            .into_iter()
            .find(|listed| listed.id == model)
            .map(|listed| listed.details)
            .unwrap_or_default()),
        ProviderKind::Ollama => {
            let client = reqwest::Client::new();
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
                details: ModelDetails { context_window: None, thinking: Some(true), tools: None },
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
                details: ModelDetails { context_window: Some(262_144), thinking: None, tools: None },
            }]
        );
        assert_eq!(next, None, "no has_more");
    }

    #[test]
    fn test_a_listing_without_data_is_an_error() {
        let error = parse_v1_models(&json(r#"{"error": "nope"}"#)).expect_err("no data array");
        assert!(error.contains("data"), "says what's missing: {error}");
    }

    #[test]
    fn test_a_real_sized_anthropic_model_keeps_its_window() {
        let body = json(r#"{"data":[{"id":"claude-x","max_input_tokens":1000000,"capabilities":{"thinking":{"supported":false}}}],"has_more":false}"#);
        let (models, _) = parse_v1_models(&body).expect("parses");
        assert_eq!(
            models[0].details,
            ModelDetails { context_window: Some(1_000_000), thinking: Some(false), tools: None }
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
            ModelDetails { context_window: Some(2048), thinking: Some(true), tools: Some(false) }
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
            ModelDetails { context_window: None, thinking: Some(false), tools: Some(true) }
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
            ModelDetails { context_window: Some(4096), thinking: Some(false), tools: Some(true) }
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
