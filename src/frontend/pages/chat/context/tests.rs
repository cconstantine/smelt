use super::*;

#[cfg(test)]
mod spend_tests {
    use super::*;
    use crate::models::ConversationSpend;

    fn spend(calls: i64, cost_usd: Option<f64>, unpriced_calls: i64) -> ConversationSpend {
        ConversationSpend { calls, cost_usd, unpriced_calls, ..ConversationSpend::default() }
    }

    #[test]
    fn test_spend_with_no_calls_says_so() {
        assert_eq!(spend_cost_text(&spend(0, None, 0)), "No completed model calls yet.");
    }

    #[test]
    fn test_spend_shows_cents_and_keeps_small_amounts_visible() {
        assert_eq!(spend_cost_text(&spend(3, Some(1.234), 0)), "$1.23");
        assert_eq!(spend_cost_text(&spend(1, Some(0.00421), 0)), "$0.0042", "not rounded away to $0.00");
        assert_eq!(spend_cost_text(&spend(2, Some(0.0), 0)), "$0.00", "a flat-rate plan's $0 prices");
    }

    #[test]
    fn test_spend_counts_calls_left_out_for_want_of_a_price() {
        assert_eq!(spend_cost_text(&spend(2, None, 2)), "No price for this model, so tokens only.");
        assert_eq!(spend_cost_text(&spend(3, Some(0.5), 1)), "$0.50, plus 1 call with no price");
        assert_eq!(spend_cost_text(&spend(5, Some(0.5), 2)), "$0.50, plus 2 calls with no price");
    }
}

/// SME-185 B37. The first tests in the repo that render markup, through
/// `dioxus::ssr` (which comes with `dioxus/server`).
#[cfg(all(test, feature = "server"))]
mod detail_body_tests {
    use super::*;

    #[test]
    fn test_a_failed_context_fetch_says_why_instead_of_loading() {
        // A transport error's text ends in a space ("Failed to fetch "),
        // which used to sit before the full stop.
        let html = dioxus::ssr::render_element(render_context_detail_body(Some(&Err("boom ".to_string()))));
        // SSR escapes the apostrophe.
        assert!(html.contains("Couldn&#39;t load the context: boom."), "{html}");
        assert!(!html.contains("Loading…"), "{html}");
    }

    #[test]
    fn test_the_context_view_says_loading_until_its_fetch_lands() {
        let html = dioxus::ssr::render_element(render_context_detail_body(None));
        assert!(html.contains("Loading…"), "{html}");
    }

    /// SME-185 code review 1: the heading was only in the two loaded arms, so
    /// a fetch that failed after a slow load inserted it and shifted the
    /// panel's content down. Every state renders it, and only once.
    #[test]
    fn test_the_context_dialog_keeps_its_heading_in_every_state() {
        for (state, html) in [
            ("loading", dioxus::ssr::render_element(render_context_detail_body(None))),
            (
                "failed",
                dioxus::ssr::render_element(render_context_detail_body(Some(&Err("boom".to_string())))),
            ),
        ] {
            assert_eq!(html.matches("<h3>Context</h3>").count(), 1, "{state}: {html}");
        }
    }
}

/// SME-277's SSR tests of the loaded-file, System prompt and Tools panes:
/// the same rule the trust card uses, one notice per pane that hides
/// something, and no raw invisible character anywhere in the html. The
/// existing `detail_body_tests` above stay green unedited: the loading
/// and failed states render no file content at all.
#[cfg(all(test, feature = "server"))]
mod detail_shown_tests {
    use super::*;
    use crate::anthropic::ToolDefinition;
    use crate::git::ProjectInstructions;

    fn snapshot(content: &str, system: Option<&str>, tools: Vec<ToolDefinition>) -> ContextDetailSnapshot {
        ContextDetailSnapshot {
            system: system.map(|s| s.to_string()),
            instructions: vec![ProjectInstructions {
                repo_url: "file:///tmp/v.git".to_string(),
                path: "/workspace/v/AGENTS.md".to_string(),
                commit: None,
                content: content.to_string(),
                file_bytes: content.len() as u64,
                truncated: false,
            }],
            tools,
            message_count: 0,
            usage: None,
            context_window: 200_000,
            spend: Default::default(),
            prices_as_of: None,
            group_spend: Default::default(),
            group_root_title: None,
        }
    }

    /// The pane pair: the file's U+200B and the same character inside the
    /// System prompt (which embeds the file). Today both reach the html
    /// raw; each pane must show its escape with its own notice, and no
    /// raw character anywhere.
    #[test]
    fn test_the_file_and_system_prompt_panes_escape_hidden_characters_with_notices() {
        let snap = snapshot("Be kind.\u{200b}", Some("… instructions: Be kind.\u{200b} …"), vec![]);
        let html = dioxus::ssr::render_element(render_context_detail_body(Some(&Ok(snap))));
        assert_eq!(html.matches("\\u{200b}").count(), 2, "{html}");
        assert_eq!(html.matches("1 character here").count(), 2, "{html}");
        assert!(!html.contains('\u{200b}'), "raw U+200B in the html: {html}");
    }

    /// The Tools panes (the user's Q3 answer): a connected server's tool
    /// description and input schema, escaped with one count for the tool
    /// — 1 in the description, 2 in the schema text.
    #[test]
    fn test_the_tools_panes_escape_hidden_characters_with_one_notice() {
        let tools = vec![ToolDefinition {
            name: "example".to_string(),
            description: "does things\u{200b}".to_string(),
            input_schema: serde_json::json!({"a\u{200b}b": "t\u{200b}ext"}),
        }];
        let snap = snapshot("plain file", Some("plain prompt"), tools);
        let html = dioxus::ssr::render_element(render_context_detail_body(Some(&Ok(snap))));
        assert!(html.contains("3 characters here"), "{html}");
        assert!(html.contains("\\u{200b}"), "{html}");
        assert!(!html.contains('\u{200b}'), "raw U+200B in the html: {html}");
    }

    /// Review 1, finding 1 (SME-277): the tool's *name* is the same
    /// server-written text in the same pane, and it reached the html raw
    /// while its description and schema were escaped either side of it.
    /// Q3's answer was "escape the Tools panes"; a name is part of one.
    #[test]
    fn test_a_tools_name_with_a_hidden_character_is_escaped_too() {
        let tools = vec![ToolDefinition {
            name: "mcp__serv\u{200b}er__echo".to_string(),
            description: "plain".to_string(),
            input_schema: serde_json::json!({"a": "b"}),
        }];
        let snap = snapshot("plain file", Some("plain prompt"), tools);
        let html = dioxus::ssr::render_element(render_context_detail_body(Some(&Ok(snap))));
        assert!(!html.contains('\u{200b}'), "raw U+200B in the tool name: {html}");
        assert!(html.contains("\\u{200b}"), "{html}");
        assert!(html.contains("1 character here"), "{html}");
    }

    /// The no-regression cell for the context view: nothing hidden, no
    /// notice anywhere, text exactly as today.
    #[test]
    fn test_the_context_view_shows_no_notice_for_a_file_with_nothing_hidden() {
        let snap = snapshot("Be kind.\n日本 é", Some("You are smelt."), vec![]);
        let html = dioxus::ssr::render_element(render_context_detail_body(Some(&Ok(snap))));
        assert!(!html.contains("render as nothing"), "{html}");
        assert!(html.contains("Be kind.\n日本 é"), "{html}");
        assert!(html.contains("You are smelt."), "{html}");
    }
}

#[cfg(test)]
mod context_usage_tests {
    use super::*;

    #[test]
    fn test_context_usage_percent_none_when_usage_not_yet_known() {
        let snapshot = ContextUsageSnapshot {
            usage: None,
            context_window: 200_000,
        };
        assert_eq!(context_usage_percent(&snapshot), None);
    }

    #[test]
    fn test_context_usage_percent_computes_input_plus_output_over_window() {
        let snapshot = ContextUsageSnapshot {
            usage: Some(TokenUsage {
                input_tokens: 40_000,
                output_tokens: 10_000,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            }),
            context_window: 200_000,
        };
        assert_eq!(context_usage_percent(&snapshot), Some(25));
    }

    #[test]
    fn test_context_usage_percent_clamps_at_100() {
        let snapshot = ContextUsageSnapshot {
            usage: Some(TokenUsage {
                input_tokens: 500_000,
                output_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            }),
            context_window: 200_000,
        };
        assert_eq!(context_usage_percent(&snapshot), Some(100));
    }

    #[test]
    fn test_context_usage_percent_counts_cache_tokens_too() {
        // Cache tokens are additive to input_tokens (Anthropic counts fresh
        // vs. cached input separately) — they still occupy real context
        // window space and must count toward "how full is it", not be
        // silently excluded.
        let snapshot = ContextUsageSnapshot {
            usage: Some(TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_input_tokens: 25_000,
                cache_read_input_tokens: 25_000,
            }),
            context_window: 200_000,
        };
        assert_eq!(context_usage_percent(&snapshot), Some(25));
    }

    #[test]
    fn test_context_usage_breakdown_splits_every_category_and_computes_free() {
        let usage = TokenUsage {
            input_tokens: 40_000,
            output_tokens: 10_000,
            cache_creation_input_tokens: 5_000,
            cache_read_input_tokens: 5_000,
        };
        let breakdown = context_usage_breakdown(&usage, 200_000);
        assert_eq!(
            breakdown,
            ContextUsageBreakdown {
                input_tokens: 40_000,
                output_tokens: 10_000,
                cache_creation_tokens: 5_000,
                cache_read_tokens: 5_000,
                free_tokens: 140_000,
                context_window: 200_000,
            }
        );
    }

    #[test]
    fn test_context_usage_breakdown_free_never_negative_when_usage_exceeds_window() {
        let usage = TokenUsage {
            input_tokens: 150_000,
            output_tokens: 100_000,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        let breakdown = context_usage_breakdown(&usage, 200_000);
        assert_eq!(breakdown.free_tokens, 0);
    }
}
