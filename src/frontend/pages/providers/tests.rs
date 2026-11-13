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

/// A `ModelInfo` for these tests: nothing overridden, nothing learned,
/// and only the tools tell — the learned window is what the tests below
/// it vary.
fn model_row(id: &str, tools: Option<bool>) -> ModelInfo {
    ModelInfo {
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
        learned_context_window: None,
        learned_window_at: None,
        learned_window_reason: None,
        learned_window_conversation: None,
        details_error: None,
    }
}

#[test]
fn test_models_without_tools_are_suggested_last() {
    let model = |id: &str, tools: Option<bool>| model_row(id, tools);
    let ordered = suggestion_order(vec![model("a", Some(false)), model("b", None), model("c", Some(true))]);
    let ids: Vec<&str> = ordered.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["b", "c", "a"]);
}

/// SME-439 PR 2: when smelt learned a model's window, the page says the
/// whole story — value, telling, when, conversation — and always shows it
/// when one exists, the user's own value notwithstanding.
#[test]
fn test_the_learned_line_tells_its_whole_story() {
    let learned = ModelInfo {
        learned_context_window: Some(160_799),
        learned_window_at: Some("2026-11-06T16:14:07".to_string()),
        learned_window_reason: Some("window_stop".to_string()),
        learned_window_conversation: Some(2_294_639_607),
        ..model_row("strata", None)
    };
    assert_eq!(
        learned_window_line(&learned).as_deref(),
        Some("smelt learned 160,799 after a window stop on Nov 6 16:14 (conversation #2294639607)")
    );
    let refused = ModelInfo { learned_window_reason: Some("refusal".to_string()), ..learned.clone() };
    assert_eq!(
        learned_window_line(&refused).as_deref(),
        Some("smelt learned 160,799 after a named refusal on Nov 6 16:14 (conversation #2294639607)")
    );
    let ps = ModelInfo { learned_window_reason: Some("ollama_ps".to_string()), ..learned.clone() };
    assert!(
        learned_window_line(&ps).as_deref().is_some_and(|l| l.starts_with("smelt learned 160,799 after Ollama's answer")),
        "{:?}",
        learned_window_line(&ps)
    );
    let untold = ModelInfo { learned_window_reason: None, learned_window_at: None, learned_window_conversation: None, ..learned.clone() };
    assert_eq!(learned_window_line(&untold).as_deref(), Some("smelt learned 160,799 from a window measurement"), "a telling that cannot be read leaves plain words, not a crash");
}

/// Nothing learned, nothing said.
#[test]
fn test_the_learned_line_is_absent_with_nothing_learned() {
    assert_eq!(learned_window_line(&model_row("m", None)), None);
}

/// The incident made visible: with the user's own Context window also
/// set, the learned line still appears and admits what the reader does
/// not use. A field value the reader would ignore (zero) gets no such
/// tag (Decided (Driver): the tag follows the read path's positivity).
#[test]
fn test_the_learned_line_admits_when_the_users_value_wins() {
    let masked = ModelInfo {
        learned_context_window: Some(160_799),
        learned_window_reason: Some("window_stop".to_string()),
        context_window_override: Some(262_144),
        ..model_row("strata", None)
    };
    assert_eq!(
        learned_window_line(&masked).as_deref(),
        Some("smelt learned 160,799 after a window stop (not in use — your Context window value wins)")
    );
    let nonsense = ModelInfo { context_window_override: Some(0), ..masked.clone() };
    assert_eq!(
        learned_window_line(&nonsense).as_deref(),
        Some("smelt learned 160,799 after a window stop"),
        "a value the reader ignores cannot win the read"
    );
}

/// SME-162: "Max concurrent requests" as sent: blank is no limit, and
/// (review 1) anything but a whole number, 0 or more, is refused rather
/// than saved as no limit.
#[test]
fn test_max_concurrent_requests_reads_blank_zero_and_a_count() {
    assert_eq!(max_requests_from_text(""), Ok(None), "blank is no limit");
    assert_eq!(max_requests_from_text("  "), Ok(None));
    assert_eq!(max_requests_from_text("0"), Ok(Some(0)), "0 is the server's own count");
    assert_eq!(max_requests_from_text(" 3 "), Ok(Some(3)));
    for typed in ["abc", "1.5", "-1", "99999999999"] {
        let refused = max_requests_from_text(typed).expect_err(typed);
        assert!(refused.contains("Max concurrent requests"), "{refused}");
    }
}

/// SME-123 DR12: every row's key badge says the same thing, with the
/// ending where there's one to show.
#[test]
fn test_a_key_badge_reads_the_same_with_or_without_its_ending() {
    use super::key_badge;
    assert_eq!(key_badge(Some("\u{2026}5ENp")), "Key saved \u{2026}5ENp");
    assert_eq!(key_badge(None), "Key saved");
}

/// SME-123 review 2 M1: with nothing typed, choosing takes the first
/// listed model (the placeholder); with nothing listed either, nothing.
#[test]
fn test_an_empty_model_field_chooses_the_first_listed_model() {
    use super::picker::model_to_choose;
    assert_eq!(model_to_choose("", Some("first")), Some("first".to_string()));
    assert_eq!(model_to_choose("  ", Some("first")), Some("first".to_string()));
    assert_eq!(model_to_choose(" typed ", Some("first")), Some("typed".to_string()));
    assert_eq!(model_to_choose("", None), None);
}

/// A window as served, `hours` from its reset at `now`.
fn usage_window(name: &str, utilization: f64, status: &str, hours: i64, now: chrono::DateTime<chrono::Utc>) -> crate::claude_cli::usage::UsageWindow {
    crate::claude_cli::usage::UsageWindow {
        name: name.to_string(),
        utilization,
        status: status.to_string(),
        resets_at: Some(now + chrono::Duration::hours(hours)),
        seen_at: now - chrono::Duration::minutes(3),
        reset_passed: hours < 0,
        elapsed_fraction: None,
        projected: None,
    }
}

fn usage_of(windows: Vec<crate::claude_cli::usage::UsageWindow>, now: chrono::DateTime<chrono::Utc>) -> crate::claude_cli::usage::ProviderUsage {
    crate::claude_cli::usage::ProviderUsage { provider_id: 1, windows, observed_at: now }
}

fn usage_now() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-11-13T12:00:00Z").expect("a time").to_utc()
}

/// SME-486: the short text puts 5-hour before 7-day whatever the order
/// served, and an ended window reads as a dash.
#[test]
fn test_the_usage_short_text_orders_and_dashes() {
    use super::usage::usage_short_text;
    let now = usage_now();
    let usage = usage_of(vec![usage_window("seven_day", 0.32, "allowed", 84, now), usage_window("five_hour", 0.454, "allowed", 2, now)], now);
    assert_eq!(usage_short_text(&usage), "5h 45% \u{b7} 7d 32%");
    let ended = usage_of(vec![usage_window("five_hour", 0.9, "allowed", -1, now), usage_window("seven_day", 1.0, "rejected", 84, now)], now);
    assert_eq!(usage_short_text(&ended), "5h \u{2014} \u{b7} 7d 100%");
    assert_eq!(usage_short_text(&usage_of(vec![], now)), "");
}

/// SME-486: the indicator's colour, at the plan's thresholds; an ended
/// window counts for nothing.
#[test]
fn test_the_usage_level_follows_the_thresholds() {
    use super::usage::usage_level;
    let now = usage_now();
    let one = |w| usage_of(vec![w], now);
    assert_eq!(usage_level(&one(usage_window("five_hour", 0.69, "allowed", 2, now))), None);
    assert_eq!(usage_level(&one(usage_window("five_hour", 0.70, "allowed", 2, now))), Some("warn"));
    assert_eq!(usage_level(&one(usage_window("five_hour", 0.10, "allowed_warning", 2, now))), Some("warn"));
    assert_eq!(usage_level(&one(usage_window("seven_day", 0.90, "allowed", 84, now))), Some("high"));
    assert_eq!(usage_level(&one(usage_window("seven_day", 0.10, "rejected", 84, now))), Some("high"));
    assert_eq!(usage_level(&one(usage_window("five_hour", 0.95, "rejected", -1, now))), None, "the reset has passed");
    let mut paced = usage_window("seven_day", 0.40, "allowed", 84, now);
    paced.projected = Some(1.0);
    assert_eq!(usage_level(&one(paced.clone())), Some("warn"), "on pace for the cap");
    paced.projected = Some(0.99);
    assert_eq!(usage_level(&one(paced)), None);
}

/// SME-486: a reset within a day says its time, a later one its weekday
/// too; "seen" counts minutes, hours, then days.
#[test]
fn test_reset_and_seen_texts() {
    use super::usage::{reset_text, seen_text, usage_title};
    let now = usage_now();
    assert_eq!(reset_text(&usage_window("five_hour", 0.4, "allowed", 2, now), now), "resets 14:00 UTC");
    assert_eq!(reset_text(&usage_window("seven_day", 0.4, "allowed", 84, now), now), "resets Tue 00:00 UTC");
    assert_eq!(reset_text(&usage_window("five_hour", 0.4, "allowed", -1, now), now), "reset since");
    assert_eq!(seen_text(now, now), "seen just now");
    assert_eq!(seen_text(now - chrono::Duration::minutes(3), now), "seen 3 min ago");
    assert_eq!(seen_text(now - chrono::Duration::minutes(130), now), "seen 2 h ago");
    assert_eq!(seen_text(now - chrono::Duration::days(4), now), "seen 4 d ago");
    let usage = usage_of(vec![usage_window("seven_day", 0.32, "allowed", 84, now), usage_window("five_hour", 0.45, "allowed", 2, now)], now);
    assert_eq!(usage_title(&usage), "Claude subscription: 5-hour 45%, resets 14:00 UTC; 7-day 32%, resets Tue 00:00 UTC");
}
