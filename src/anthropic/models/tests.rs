// The `#[cfg(test)] mod tests` of `src/anthropic/models.rs`, moved whole
// (SME-73 commit 0): one dedent, and `fixtures/` became `../fixtures/`.
use super::*;
use crate::anthropic::stream::Auth;

fn json(text: &str) -> Value {
    serde_json::from_str(text).expect("a fixture is JSON")
}

const ANTHROPIC: &str = include_str!("../fixtures/anthropic_v1_models.json");
const LLAMA_CPP: &str = include_str!("../fixtures/llama_cpp_v1_models.json");
const LLAMA_CPP_PROPS: &str = include_str!("../fixtures/llama_cpp_props.json");
const OLLAMA_TAGS: &str = include_str!("../fixtures/ollama_api_tags.json");
const OLLAMA_SHOW: &str = include_str!("../fixtures/ollama_api_show.json");
const OLLAMA_PS: &str = include_str!("../fixtures/ollama_api_ps.json");
const HAIKU_4_5: &str = include_str!("../fixtures/anthropic_v1_models_claude_haiku_4_5.json");
const HAIKU_5_5: &str = include_str!("../fixtures/anthropic_v1_models_claude_haiku_5_5.json");

/// B9 (SME-122): Anthropic's own entries for Haiku 4.5 and 5.5, as
/// captured: 4.5 can't take adaptive thinking, 5.5 can.
#[test]
fn test_anthropics_entries_say_which_models_take_adaptive_thinking() {
    let haiku_4_5 = parse_v1_model(&json(HAIKU_4_5)).expect("parses");
    assert_eq!(haiku_4_5.id, "claude-haiku-4-5-20251001");
    assert_eq!((haiku_4_5.details.thinking, haiku_4_5.details.adaptive_thinking), (Some(true), Some(false)));
    let haiku_5_5 = parse_v1_model(&json(HAIKU_5_5)).expect("parses");
    assert_eq!((haiku_5_5.details.thinking, haiku_5_5.details.adaptive_thinking), (Some(true), Some(true)));
}

#[test]
fn test_anthropics_listing_gives_names_thinking_and_the_next_page() {
    let (models, next) = parse_v1_models(&json(ANTHROPIC)).expect("parses");
    assert_eq!(
        models,
        vec![ListedModel {
            id: "claude-opus-5".to_string(),
            display_name: Some("Claude Opus 5".to_string()),
            // The docs' example says 0 tokens: a placeholder, not a size.
            details: ModelDetails { adaptive_thinking: Some(true), context_window: None, thinking: Some(true), tools: None, max_output: None },
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
            details: ModelDetails { adaptive_thinking: None, context_window: Some(262_144), thinking: None, tools: None, max_output: None },
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
        details: ModelDetails { adaptive_thinking: None, context_window: window, thinking: None, tools: None, max_output: None },
    }
}

#[test]
fn test_props_apply_to_the_model_the_server_serves() {
    let info = parse_llama_props(&json(LLAMA_CPP_PROPS));
    let mut one = vec![listed("anything", Some(4096))];
    apply_llama_props(&mut one, &info);
    assert_eq!(one[0].details, ModelDetails { adaptive_thinking: None, context_window: Some(262_144), thinking: None, tools: Some(true), max_output: None }, "the only one listed");

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
        ModelDetails { adaptive_thinking: None, context_window: Some(1_000_000), thinking: Some(false), tools: None, max_output: None }
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
        ModelDetails { adaptive_thinking: None, context_window: Some(2048), thinking: Some(true), tools: Some(false), max_output: None }
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
        ModelDetails { adaptive_thinking: None, context_window: None, thinking: Some(false), tools: Some(true), max_output: None }
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
        slots_of: None,
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
        slots_of: None,
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
        slots_of: None,
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
        ModelDetails { adaptive_thinking: None, context_window: Some(4096), thinking: Some(false), tools: Some(true), max_output: None }
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
    assert_eq!(details, ModelDetails { adaptive_thinking: None, context_window: Some(1_000_000), thinking: Some(true), tools: None, max_output: None });
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
        slots_of: None,
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
