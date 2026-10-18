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
        [ProviderKind::Anthropic, ProviderKind::Ollama, ProviderKind::LlamaCpp, ProviderKind::Other],
        "listed between Ollama and Other"
    );
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
