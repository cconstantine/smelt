use super::*;

#[cfg(feature = "server")]
#[test]
fn test_a_secret_hint_shows_only_the_last_four_characters() {
    assert_eq!(secret_hint("sk-ant-api03-abcdefWXYZ"), Some("\u{2026}WXYZ".to_string()));
    assert_eq!(secret_hint("short-key"), None, "too short to show any of");
    assert_eq!(secret_hint(""), None);
    // Characters, not bytes: a multi-byte secret doesn't split mid-char.
    assert_eq!(secret_hint("ключ-ключ-ключ"), Some("\u{2026}ключ".to_string()));
}

#[test]
fn test_kinds_and_auth_kinds_round_trip_their_names() {
    for kind in ProviderKind::ALL {
        assert_eq!(ProviderKind::parse(kind.as_str()), Some(kind));
    }
    for kind in AuthKind::ALL {
        assert_eq!(AuthKind::parse(kind.as_str()), Some(kind));
    }
    assert_eq!(ProviderKind::parse("openai"), None);
}

/// SME-439 PR 2: the page reads the learned window and its whole telling
/// — value, when, which learner, which conversation — off `ModelInfo`,
/// whether or not a user value also exists and wins the read.
#[cfg(feature = "server")]
#[test]
fn test_model_info_carries_the_learned_window_and_its_telling() {
    let at = chrono::NaiveDateTime::parse_from_str("2026-11-06 16:14:07", "%Y-%m-%d %H:%M:%S").unwrap().and_utc();
    let row = crate::db::ProviderModelRow {
        context_window: Some(262_144),
        learned_context_window: Some(160_799),
        learned_context_window_at: Some(at),
        learned_context_window_reason: Some("window_stop".to_string()),
        learned_context_window_conversation: Some(2_294_639_607),
        ..Default::default()
    };
    let info = model_info(ProviderKind::LlamaCpp, "strata".to_string(), None, Some(&row), None);
    assert_eq!(info.context_window_override, Some(262_144), "the user's value still wins the read");
    assert_eq!(info.learned_context_window, Some(160_799));
    assert_eq!(info.learned_window_reason.as_deref(), Some("window_stop"));
    assert_eq!(info.learned_window_conversation, Some(2_294_639_607));
    assert_eq!(info.learned_window_at.as_deref(), Some("2026-11-06T16:14:07"), "ISO, the page formats it");

    let bare = model_info(ProviderKind::LlamaCpp, "strata".to_string(), None, None, None);
    assert_eq!(
        (bare.learned_context_window, bare.learned_window_at, bare.learned_window_reason, bare.learned_window_conversation),
        (None, None, None, None),
        "nothing learned, nothing to show"
    );
}

#[cfg(feature = "server")]
use sqlx::PgPool;

/// A `ModelDetails` for the tests below it: only the reported window.
#[cfg(feature = "server")]
fn details_with_window(window: u32) -> crate::anthropic::models::ModelDetails {
    crate::anthropic::models::ModelDetails { context_window: Some(window), ..Default::default() }
}

/// A stored provider of `kind` at `base_url`, for the report tests below.
#[cfg(feature = "server")]
async fn report_provider(pool: &PgPool, kind: &str, base_url: &str) -> crate::db::InferenceProvider {
    crate::db::create_inference_provider(pool, "p", kind, base_url, "api_key", "k", false, None, true, None)
        .await
        .expect("provider")
}

/// `window_learn` spans only, and their one telling attribute — the
/// pattern from `turn::tests::window_learning`, copied for the clear's
/// own side of the seam (round 1 H1: the self-clears must speak).
#[cfg(feature = "server")]
fn window_learn_spans(spans: &[opentelemetry_sdk::trace::SpanData]) -> Vec<&opentelemetry_sdk::trace::SpanData> {
    spans.iter().filter(|span| span.name == "window_learn").collect()
}

#[cfg(feature = "server")]
fn span_attribute<'a>(span: &'a opentelemetry_sdk::trace::SpanData, key: &str) -> Option<&'a opentelemetry::Value> {
    let values: Vec<_> = span.attributes.iter().filter(|kv| kv.key.as_str() == key).map(|kv| &kv.value).collect();
    assert!(values.len() <= 1, "{key} recorded more than once: {values:?}");
    values.first().copied()
}

/// SME-439 PR 2 (Expected 3a): a listing report bigger than the learned
/// window says the learned one is too small — forget it. Everyone else's
/// report is per model, so 40,000 against a learned 32,768 clears; one
/// that doesn't outgrow it keeps, and the clear never reaches the user's
/// own field. Round 1 H1: the clear says so on `window_learn` — decision
/// `bigger_report`, the report and its per-request share, no conversation
/// id (a listing has none) — and says nothing when nothing clears.
#[cfg(feature = "server")]
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_bigger_report_forgets_a_smaller_learned_window(pool: PgPool) {
    let provider = report_provider(&pool, "anthropic", "http://one").await;
    crate::db::save_learned_context_window(&pool, provider.id, "http://one", "m", 32_768, "window_stop", 42, 200_000).await.expect("learn");
    crate::db::set_provider_model_overrides(&pool, provider.id, "m", None, Some(400_000), None, None, None).await.expect("user window");

    let (result, spans) = crate::telemetry::capture_spans(server::store_details(&pool, provider.id, "m", &details_with_window(16_384))).await;
    result.expect("smaller report");
    assert!(window_learn_spans(&spans).is_empty(), "a report that keeps says nothing: {:?}", window_learn_spans(&spans));
    let row = crate::db::get_provider_model(&pool, provider.id, "m").await.expect("get").expect("row");
    assert_eq!(row.learned_context_window, Some(32_768), "16,384 does not outgrow 32,768");

    let (result, spans) = crate::telemetry::capture_spans(server::store_details(&pool, provider.id, "m", &details_with_window(32_768))).await;
    result.expect("equal report");
    assert!(window_learn_spans(&spans).is_empty(), "equal exceeds nothing, and traces nothing: {:?}", window_learn_spans(&spans));
    let row = crate::db::get_provider_model(&pool, provider.id, "m").await.expect("get").expect("row");
    assert_eq!(row.learned_context_window, Some(32_768), "32,768 is not bigger than 32,768");

    let (result, spans) = crate::telemetry::capture_spans(server::store_details(&pool, provider.id, "m", &details_with_window(40_000))).await;
    result.expect("bigger report");
    let found = window_learn_spans(&spans);
    let [span] = found.as_slice() else { panic!("one window_learn span for the clear: {found:?}") };
    assert_eq!(span_attribute(span, "smelt.window.source"), Some(&"listing".into()));
    assert_eq!(span_attribute(span, "smelt.window.decision"), Some(&"bigger_report".into()));
    assert_eq!(span_attribute(span, "smelt.window.value"), Some(&40_000_i64.into()));
    assert_eq!(span_attribute(span, "smelt.window.reported"), Some(&40_000_i64.into()));
    assert_eq!(span_attribute(span, "smelt.window.reported_per_request"), Some(&40_000_i64.into()));
    assert_eq!(span_attribute(span, "conversation_id"), None, "a listing has no conversation to name");
    let row = crate::db::get_provider_model(&pool, provider.id, "m").await.expect("get").expect("row");
    assert_eq!(row.learned_context_window, None, "40,000 outgrew the learned 32,768: forgotten");
    assert_eq!(row.context_window, Some(400_000), "the user's own field is nobody of this clear's");
}

/// SME-439 PR 2 (Expected 3a) at llama.cpp's per-request reading: the
/// report is the server's total, and a llama.cpp slot gets its share —
/// 65,536 across four slots is 16,384, which does not outgrow a learned
/// 32,768; 163,840 total (40,960 a request) does. The clear is
/// `reported / total_slots`, the learner's own division.
#[cfg(feature = "server")]
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_llama_cpp_report_divides_by_its_slots_before_it_forgets(pool: PgPool) {
    let provider = report_provider(&pool, "llama_cpp", "http://llama").await;
    sqlx::query("UPDATE inference_providers SET server_caps = $2 WHERE id = $1")
        .bind(provider.id)
        .bind(serde_json::json!({ "n_ctx": 65_536, "total_slots": 4, "template_caps": {} }))
        .execute(&pool)
        .await
        .expect("caps");
    crate::db::save_learned_context_window(&pool, provider.id, "http://llama", "m", 32_768, "window_stop", 42, 200_000).await.expect("learn");

    server::store_details(&pool, provider.id, "m", &details_with_window(65_536)).await.expect("four-slot report");
    let row = crate::db::get_provider_model(&pool, provider.id, "m").await.expect("get").expect("row");
    assert_eq!(row.learned_context_window, Some(32_768), "65,536 is four slots' total — 16,384 a request, not a bigger window");

    server::store_details(&pool, provider.id, "m", &details_with_window(163_840)).await.expect("bigger four-slot report");
    let row = crate::db::get_provider_model(&pool, provider.id, "m").await.expect("get").expect("row");
    assert_eq!(row.learned_context_window, None, "40,960 a request outgrew 32,768: forgotten");
}

/// Wire types go to the browser as JSON; each shape round-trips.
#[test]
fn test_conversation_models_round_trip_through_json() {
    let choice = ModelChoice {
        provider_id: 1,
        provider_name: "Anthropic".to_string(),
        model: "claude-opus-5".to_string(),
        context_window: 200_000,
        context_window_known: true,
        tools: None,
    };
    for value in [
        ConversationModel::Chosen { choice: choice.clone() },
        ConversationModel::Default { choice },
        ConversationModel::NoDefault {},
        ConversationModel::NoProviders {},
    ] {
        let json = serde_json::to_string(&value).expect("serializes");
        assert_eq!(serde_json::from_str::<ConversationModel>(&json).expect("deserializes"), value);
    }
}

#[cfg(feature = "server")]
fn source(id: &str, api: Option<&str>) -> PriceSource {
    PriceSource { id: id.to_string(), name: id.to_string(), api: api.map(str::to_string) }
}

fn catalog_sources() -> Vec<PriceSource> {
    vec![
        source("anthropic", None),
        source("deepseek", Some("https://api.deepseek.com")),
        source("minimax", Some("https://api.minimax.io/anthropic/v1")),
        source("minimax-coding-plan", Some("https://api.minimax.io/anthropic/v1")),
        source("moonshotai", Some("https://api.moonshot.ai/v1")),
        source("zai", Some("https://api.z.ai/api/paas/v4")),
        source("zai-coding-plan", Some("https://api.z.ai/api/coding/paas/v4")),
    ]
}

/// SME-111: a llama.cpp server wants a bearer token if any, and
/// ignores `cache_control` (it keeps its own prefix cache).
#[test]
fn test_a_llama_cpp_provider_starts_with_bearer_auth_and_no_caching() {
    assert_eq!(ProviderKind::parse("llama_cpp"), Some(ProviderKind::LlamaCpp));
    assert_eq!(ProviderKind::LlamaCpp.label(), "llama.cpp");
    assert_eq!(ProviderKind::LlamaCpp.default_auth_kind(), AuthKind::Bearer);
    assert!(!ProviderKind::LlamaCpp.default_prompt_caching());
    assert_eq!(
        ProviderKind::ALL,
        [ProviderKind::Anthropic, ProviderKind::Ollama, ProviderKind::LlamaCpp, ProviderKind::Other, ProviderKind::ClaudeCli],
        "listed between Ollama and Other, with Claude Code last (SME-429)"
    );
}

/// SME-429: Claude Code is its own kind, with no address or key, priced
/// as Anthropic's API would price its calls.
#[test]
fn test_a_claude_code_provider_is_a_binary_priced_as_anthropic() {
    assert_eq!(ProviderKind::parse("claude_cli"), Some(ProviderKind::ClaudeCli));
    assert!(!ProviderKind::ClaudeCli.is_http());
    assert!(ProviderKind::ALL.iter().filter(|k| **k != ProviderKind::ClaudeCli).all(|k| k.is_http()));
    assert!(!ProviderKind::ClaudeCli.default_prompt_caching(), "the CLI marks its own cache breakpoints");
    assert_eq!(ProviderKind::ClaudeCli.base_url_placeholder(), "claude");
    let sources = [
        PriceSource { id: "anthropic".to_string(), name: "Anthropic".to_string(), api: None },
        PriceSource { id: "other".to_string(), name: "Other".to_string(), api: Some("https://claude".to_string()) },
    ];
    assert_eq!(suggest_price_source(&sources, ProviderKind::ClaudeCli, "claude").as_deref(), Some("anthropic"));
    assert_eq!(suggest_price_source(&sources[1..], ProviderKind::ClaudeCli, "claude"), None, "not in the catalog");
}

/// SME-162: the form sends a provider's max concurrent requests, and its
/// summary shows it back, automatic (`0`) included.
#[test]
fn test_a_providers_max_concurrent_requests_crosses_to_the_browser_and_back() {
    let input = ProviderInput {
        name: "Home".to_string(),
        kind: ProviderKind::LlamaCpp,
        base_url: "http://llama:8080".to_string(),
        auth_kind: AuthKind::Bearer,
        secret: String::new(),
        prompt_caching: false,
        price_catalog_provider: None,
        keep_reasoning: true,
        max_concurrent_requests: Some(0),
    };
    let json = serde_json::to_string(&input).expect("serializes");
    assert_eq!(serde_json::from_str::<ProviderInput>(&json).expect("deserializes"), input);
    let summary = ProviderSummary {
        id: 1,
        name: "Home".to_string(),
        kind: ProviderKind::LlamaCpp,
        base_url: "http://llama:8080".to_string(),
        auth_kind: AuthKind::Bearer,
        secret_hint: None,
        prompt_caching: false,
        price_catalog_provider: None,
        keep_reasoning: true,
        server_caps: None,
        max_concurrent_requests: Some(2),
    };
    let json = serde_json::to_string(&summary).expect("serializes");
    assert_eq!(serde_json::from_str::<ProviderSummary>(&json).expect("deserializes"), summary);
}

/// The server's info goes to the browser on the provider's summary.
#[test]
fn test_llama_server_info_round_trips_through_json() {
    let info = LlamaServerInfo {
        n_ctx: Some(262_144),
        total_slots: Some(1),
        model_alias: Some("flash-next".to_string()),
        build_info: Some("b1".to_string()),
        template_caps: [("supports_reasoning_effort".to_string(), true)].into_iter().collect(),
    };
    let json = serde_json::to_string(&info).expect("serializes");
    assert_eq!(serde_json::from_str::<LlamaServerInfo>(&json).expect("deserializes"), info);
    assert!(info.supports_reasoning_effort());
    assert!(!info.supports_preserve_reasoning(), "not reported");
}

#[test]
fn test_anthropics_own_api_is_priced_as_anthropic() {
    let sources = catalog_sources();
    assert_eq!(
        suggest_price_source(&sources, ProviderKind::Anthropic, "https://api.anthropic.com").as_deref(),
        Some("anthropic")
    );
    assert_eq!(
        suggest_price_source(&sources, ProviderKind::Anthropic, "https://API.anthropic.com/").as_deref(),
        Some("anthropic")
    );
    assert_eq!(
        suggest_price_source(&sources, ProviderKind::Anthropic, "http://localhost:4000"),
        None,
        "a gateway isn't Anthropic's prices"
    );
}

#[test]
fn test_another_service_is_matched_by_host_then_path() {
    let sources = catalog_sources();
    let suggest = |url| suggest_price_source(&sources, ProviderKind::Other, url);
    assert_eq!(suggest("https://api.deepseek.com/anthropic").as_deref(), Some("deepseek"));
    assert_eq!(suggest("https://api.moonshot.ai/anthropic").as_deref(), Some("moonshotai"));
    assert_eq!(suggest("https://api.z.ai/api/coding/paas/v4").as_deref(), Some("zai-coding-plan"), "nearest path");
    assert_eq!(suggest("https://api.z.ai/api/anthropic"), None, "two equally near");
    assert_eq!(suggest("https://api.minimax.io/anthropic"), None, "a service and its plan share the address");
    assert_eq!(suggest("http://llama:8080"), None);
    assert_eq!(suggest("not a url"), None);
}

mod server_tests;
