use super::*;

fn choice(tools: Option<bool>, known: bool) -> ModelChoice {
    ModelChoice {
        provider_id: 1,
        provider_name: "Home".to_string(),
        model: "gemma4".to_string(),
        context_window: 4096,
        context_window_known: known,
        tools,
    }
}

#[test]
fn test_a_choice_warns_about_missing_tools_and_an_unknown_window() {
    assert_eq!(choice_label(&choice(None, true)), "Home \u{b7} gemma4");
    assert!(choice_warnings(&choice(None, true)).is_empty());
    assert!(choice_warnings(&choice(Some(true), true)).is_empty());
    let warnings = choice_warnings(&choice(Some(false), false));
    assert_eq!(warnings.len(), 2);
    assert!(warnings[0].contains("tools"), "{warnings:?}");
    assert!(warnings[1].contains("200,000"), "{warnings:?}");
}

/// SME-72 review: "provider's default" is a choice of its own, so
/// saving a context window doesn't pin thinking.
#[test]
fn test_thinking_choices_round_trip_including_no_override() {
    for thinking in [None, Some(true), Some(false)] {
        assert_eq!(thinking_from_choice(thinking_choice(thinking)), thinking);
    }
    assert_eq!(thinking_label("default", Some(false)), "Provider's default (off)");
}

/// SME-111: llama.cpp's Effort offers what its template takes, and
/// only when its template reads effort at all.
#[test]
fn test_effort_choices_follow_the_kind_and_the_template() {
    use crate::anthropic::Effort;
    let caps = |effort: bool| LlamaServerInfo {
        template_caps: [("supports_reasoning_effort".to_string(), effort)].into_iter().collect(),
        ..Default::default()
    };
    assert_eq!(effort_choices(ProviderKind::LlamaCpp, Some(&caps(true))), vec![Effort::Low, Effort::Medium, Effort::High]);
    assert!(effort_choices(ProviderKind::LlamaCpp, Some(&caps(false))).is_empty());
    assert!(effort_choices(ProviderKind::LlamaCpp, None).is_empty(), "never read");
    assert_eq!(effort_choices(ProviderKind::Anthropic, None), Effort::ALL.to_vec());
}

#[test]
fn test_a_llama_cpp_servers_line_says_what_it_reported() {
    let caps = LlamaServerInfo {
        n_ctx: Some(262_144),
        total_slots: Some(1),
        model_alias: Some("flash-next".to_string()),
        build_info: Some("b11434-5e03bdd87".to_string()),
        template_caps: [
            ("supports_reasoning_effort".to_string(), true),
            ("supports_preserve_reasoning".to_string(), true),
            ("supports_tool_calls".to_string(), true),
            ("supports_system_role".to_string(), true),
        ]
        .into_iter()
        .collect(),
    };
    assert_eq!(
        llama_server_summary(&caps),
        "llama.cpp b11434 \u{b7} flash-next \u{b7} 1 slot \u{b7} 262,144-token window \u{b7} template: preserve reasoning, reasoning effort, tool calls"
    );
    assert_eq!(
        llama_server_summary(&LlamaServerInfo::default()),
        "llama.cpp \u{b7} template: no reasoning effort or preserved reasoning"
    );
}

/// SME-111 review 1: a slow probe of an old address doesn't hide the
/// suggestion for the address in the field.
#[test]
fn test_only_the_current_addresss_probe_counts() {
    let b = "http://b".to_string();
    assert_eq!(after_probe(None, b.clone(), true, "http://b"), Some(b.clone()));
    assert_eq!(after_probe(Some(b.clone()), "http://a".to_string(), false, "http://b"), Some(b.clone()), "a stale no");
    assert_eq!(after_probe(None, "http://a".to_string(), true, "http://b"), None, "a stale yes");
    assert_eq!(after_probe(Some(b.clone()), b, false, "http://b"), None, "the current address said no");
}

#[test]
fn test_a_token_field_is_blank_or_a_number() {
    assert_eq!(token_field("  ", "Max reply tokens"), Ok(None));
    assert_eq!(token_field("131,072", "Max reply tokens"), Ok(Some(131_072)));
    assert_eq!(token_field("lots", "Max reply tokens"), Err("Max reply tokens is a number of tokens.".to_string()));
}

#[test]
fn test_digits_are_grouped_by_thousands() {
    assert_eq!(group_digits(0), "0");
    assert_eq!(group_digits(999), "999");
    assert_eq!(group_digits(4096), "4,096");
    assert_eq!(group_digits(200_000), "200,000");
    assert_eq!(group_digits(1_048_576), "1,048,576");
}

#[test]
fn test_models_without_tools_are_suggested_last() {
    let model = |id: &str, tools: Option<bool>| ModelInfo {
        id: id.to_string(),
        display_name: None,
        thinking_override: None,
        context_window_override: None,
        effort_override: None,
        max_output_override: None,
        reasoning_budget_override: None,
        max_output: None,
        reported_context_window: None,
        reported_tools: tools,
        thinking: true,
        context_window: 1,
        context_window_known: true,
        details_error: None,
    };
    let ordered = suggestion_order(vec![model("a", Some(false)), model("b", None), model("c", Some(true))]);
    let ids: Vec<&str> = ordered.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["b", "c", "a"]);
}
