//! The context usage meter and its detail dialog.

use super::*;
use super::shown_text::{hidden_text_notice, shown_pre};

/// Where a loaded `AGENTS.md` came from, for the context detail view.
pub(super) fn instructions_source(doc: &crate::git::ProjectInstructions) -> String {
    let commit: String = doc.commit.as_deref().unwrap_or("").chars().take(7).collect();
    let origin = if commit.is_empty() {
        doc.repo_url.clone()
    } else {
        format!("{} at {commit}", doc.repo_url)
    };
    if doc.truncated {
        format!("{origin} \u{b7} {} bytes, only the first 32 KiB loaded", doc.file_bytes)
    } else {
        format!("{origin} \u{b7} {} bytes", doc.file_bytes)
    }
}

/// Focus the context view's first and last controls, for the sentinels
/// that keep Tab inside it.
pub(super) const FOCUS_FIRST_IN_CONTEXT_DETAIL: &str = "document.querySelector('.context-detail-close')?.focus();";

pub(super) const FOCUS_LAST_IN_CONTEXT_DETAIL: &str = "const p = document.querySelector('.context-detail-panel'); \
     if (p) { const f = p.querySelectorAll('button, summary, a[href], input, select, textarea, [tabindex=\"0\"]'); \
     (f[f.length - 1] || p).focus(); }";

/// How full the model's context window is, as a whole-number percent — the
/// always-visible indicator's own number. `None` if `usage` hasn't arrived
/// yet (a brand-new conversation). Clamped to 100 — a conversation caught
/// mid-compaction, or a `context_window` estimate that's simply wrong for
/// the configured model, shouldn't render a bar past full. See
/// SME-18.
pub(super) fn context_usage_percent(snapshot: &ContextUsageSnapshot) -> Option<u32> {
    let usage = snapshot.usage.as_ref()?;
    if snapshot.context_window == 0 {
        return None;
    }
    let breakdown = context_usage_breakdown(usage, snapshot.context_window);
    let used = breakdown
        .context_window
        .saturating_sub(breakdown.free_tokens);
    let percent = used.saturating_mul(100) / breakdown.context_window;
    Some(percent.min(100) as u32)
}

/// One category's worth of a context-window breakdown — the detail view's
/// visual meter splits usage into exactly these segments. Cache tokens are
/// additive to `input_tokens` (Anthropic counts fresh vs. cached input
/// separately) but still occupy real context-window space, so all four
/// count toward `free_tokens`'s subtraction — the same total
/// `context_usage_percent` reports, just split by category here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct ContextUsageBreakdown {
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) cache_creation_tokens: u64,
    pub(super) cache_read_tokens: u64,
    pub(super) free_tokens: u64,
    pub(super) context_window: u64,
}

pub(super) fn context_usage_breakdown(usage: &TokenUsage, context_window: u32) -> ContextUsageBreakdown {
    let input_tokens = usage.input_tokens.max(0) as u64;
    let output_tokens = usage.output_tokens.max(0) as u64;
    let cache_creation_tokens = usage.cache_creation_input_tokens.max(0) as u64;
    let cache_read_tokens = usage.cache_read_input_tokens.max(0) as u64;
    let window = context_window as u64;
    let used = input_tokens + output_tokens + cache_creation_tokens + cache_read_tokens;
    ContextUsageBreakdown {
        input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
        free_tokens: window.saturating_sub(used),
        context_window: window,
    }
}

/// The detail view's visual context-usage meter — a horizontal stacked bar,
/// one segment per non-zero category in `breakdown`, sized proportionally
/// (`flex-grow` set to each segment's own token count, so the segments
/// plus the free remainder always sum to the full bar with no manual
/// percent math). Fixed category order (input, output, cache creation,
/// cache read, free), never reassigned by size — see
/// SME-18 and the dataviz skill's
/// categorical-color rule. A zero-valued category is skipped entirely
/// (not rendered at flex-grow: 0) so it can't leave a stray 2px gap next
/// to nothing.
pub(super) fn render_context_meter(breakdown: &ContextUsageBreakdown) -> Element {
    let segment = |category: &'static str, tokens: u64, label: &str| {
        if tokens == 0 {
            return rsx! {};
        }
        let percent = if breakdown.context_window > 0 {
            tokens.saturating_mul(100) / breakdown.context_window
        } else {
            0
        };
        rsx! {
            div {
                key: "{category}",
                class: "context-meter-segment",
                "data-category": category,
                style: "flex-grow: {tokens}",
                tabindex: 0,
                role: "img",
                "aria-label": "{label}: {tokens} tokens, {percent}% of context",
                span { class: "context-meter-tooltip", "{label}: {tokens} ({percent}%)" }
            }
        }
    };

    rsx! {
        div { class: "context-meter",
            div { class: "context-meter-track",
                {segment("input", breakdown.input_tokens, "Input")}
                {segment("output", breakdown.output_tokens, "Output")}
                {segment("cache-creation", breakdown.cache_creation_tokens, "Cache creation")}
                {segment("cache-read", breakdown.cache_read_tokens, "Cache read")}
                {segment("free", breakdown.free_tokens, "Free")}
            }
            div { class: "context-meter-legend",
                if breakdown.input_tokens > 0 {
                    span { class: "context-meter-legend-item",
                        span { class: "context-meter-swatch", "data-category": "input" }
                        "Input"
                    }
                }
                if breakdown.output_tokens > 0 {
                    span { class: "context-meter-legend-item",
                        span { class: "context-meter-swatch", "data-category": "output" }
                        "Output"
                    }
                }
                if breakdown.cache_creation_tokens > 0 {
                    span { class: "context-meter-legend-item",
                        span { class: "context-meter-swatch", "data-category": "cache-creation" }
                        "Cache creation"
                    }
                }
                if breakdown.cache_read_tokens > 0 {
                    span { class: "context-meter-legend-item",
                        span { class: "context-meter-swatch", "data-category": "cache-read" }
                        "Cache read"
                    }
                }
                if breakdown.free_tokens > 0 {
                    span { class: "context-meter-legend-item",
                        span { class: "context-meter-swatch", "data-category": "free" }
                        "Free"
                    }
                }
            }
        }
    }
}

/// What the conversation's completed calls cost, for the detail view
/// (SME-106).
fn spend_cost_text(spend: &crate::models::ConversationSpend) -> String {
    crate::models::cost_text(spend.calls, spend.cost_usd, spend.unpriced_calls)
}

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

/// One notice per tool (the user's answer to SME-277's Q3): the name, the
/// description and the schema are all text the connected server writes, so
/// all three are escaped by the same rule as the panes around them, and the
/// count covers the three together. Review round 1, finding 1: the name was
/// the one left raw, reaching the html while the other two were escaped on
/// either side of it.
fn tool_notice(tool: &crate::anthropic::ToolDefinition) -> Element {
    let hidden = crate::git::hidden_characters(&tool.name)
        + crate::git::hidden_characters(&tool.description)
        + crate::git::hidden_characters(&tool.input_schema.to_string());
    if hidden > 0 {
        hidden_text_notice(hidden)
    } else {
        rsx! {}
    }
}

/// The context dialog's body: "Loading…" until its fetch lands, then the
/// snapshot, or why it couldn't load (SME-185 B37: a failed fetch used to
/// leave "Loading…" for good). A plain function, so a test can render it.
///
/// The heading is outside the match: every state has it, so a fetch that
/// fails after a slow load doesn't shift the panel's content down (code
/// review 1), and it keeps the × (placed top right) clear of the text.
pub(super) fn render_context_detail_body(detail: Option<&Result<ContextDetailSnapshot, String>>) -> Element {
    let body = match detail {
        None => rsx! { p { "Loading…" } },
        // A transport error's text ends in a space ("Failed to fetch ").
        Some(Err(reason)) => rsx! {
            ErrorText { message: Some(format!("Couldn't load the context: {}. Close it and open it again to retry.", reason.trim_end())) }
        },
        Some(Ok(detail)) => rsx! {
            if !detail.instructions.is_empty() {
                h4 { class: "context-detail-heading", "Project instructions ({detail.instructions.len()})" }
                p { class: "muted", "AGENTS.md files from this conversation's repos, sent with every turn as part of the system prompt below." }
                for doc in &detail.instructions {
                    details { class: "context-detail-instructions",
                        summary {
                            code { "{doc.path}" }
                            span { class: "muted", " {instructions_source(doc)}" }
                        }
                        {shown_pre(&doc.content, Some("context-detail-prompt"))}
                    }
                }
            }
            h4 { class: "context-detail-heading", "System prompt" }
            if let Some(system) = &detail.system {
                // Its own line breaks and headings, not one
                // run-together paragraph (SME-41 D7).
                {shown_pre(system, Some("context-detail-prompt"))}
            } else {
                p { class: "muted", "None set." }
            }
            p { "Messages: {detail.message_count}" }
            if let Some(usage) = &detail.usage {
                p {
                    "Tokens — input: {usage.input_tokens}, output: {usage.output_tokens}, "
                    "cache creation: {usage.cache_creation_input_tokens}, cache read: {usage.cache_read_input_tokens}"
                }
                {render_context_meter(&context_usage_breakdown(usage, detail.context_window))}
            }
            p { "Context window: {detail.context_window}" }
            h4 { class: "context-detail-heading", "Spent so far" }
            p { "{spend_cost_text(&detail.spend)}" }
            if detail.spend.calls > 0 {
                p {
                    "{crate::models::calls_text(detail.spend.calls)} — uncached input: {detail.spend.input_tokens}, "
                    "cache write: {detail.spend.cache_creation_input_tokens}, cache read: {detail.spend.cache_read_input_tokens}, "
                    "output: {detail.spend.output_tokens}"
                }
                p { class: "muted",
                    "A stopped or failed call is counted with what it used before it ended, estimated where the provider "
                    "hadn't said yet: its output, and its input from the request's size when the provider reports that "
                    "only at the end (LiteLLM). Each call is priced from models.dev's list "
                    "when it finished, without time-of-day surcharges such as DeepSeek's peak hours."
                    if let Some(at) = detail.prices_as_of {
                        {format!(" Prices as of {} UTC.", at.format("%Y-%m-%d %H:%M"))}
                    }
                }
            }
            h4 { "Tools ({detail.tools.len()})" }
            for tool in &detail.tools {
                div { class: "context-detail-tool",
                    strong { "{crate::git::plain_multiline_text(&tool.name)}" }
                    {tool_notice(tool)}
                    p { "{crate::git::plain_multiline_text(&tool.description)}" }
                    pre { "{crate::git::plain_multiline_text(&tool.input_schema.to_string())}" }
                }
            }
        },
    };
    rsx! {
        h3 { "Context" }
        {body}
    }
}

/// The context usage bar above the transcript and, when it's clicked,
/// the context detail dialog (SME-18, SME-82).
#[component]
pub(super) fn ContextUsage(
    selected: Memo<Option<i64>>,
    state: Store<ConversationState>,
    mut context_bar_el: Signal<Option<MountedEvent>>,
) -> Element {
    let context_usage = state.context_usage();
    let mut context_detail = state.context_detail();
    let mut context_detail_open = state.context_detail_open();
    // Closes the detail view from inside it (its ×, Escape, a click
    // outside) and puts focus back on the bar that opened it.
    let mut close_context_detail = move || {
        context_detail_open.set(false);
        if let Some(bar) = context_bar_el.peek().clone() {
            spawn(async move {
                let _ = bar.set_focus(true).await;
            });
        }
    };

    // Counts the opens, so only the latest one's fetch lands (SME-185 B37).
    let mut detail_request = use_signal(|| 0u64);

    // Fetches a fresh detail snapshot every time it's opened, rather than
    // caching — matches the idea's "most recently sent request" decision;
    // reopening after a new turn should show that turn's numbers, not a
    // stale first-open snapshot.
    let mut open_context_detail = move || {
        let Some(id) = selected() else { return };
        context_detail_open.set(true);
        // Not the last snapshot, possibly another conversation's (SME-51 B11).
        context_detail.set(None);
        let request = *detail_request.peek() + 1;
        detail_request.set(request);
        spawn(async move {
            // A failure is shown too, not dropped (SME-185 B37). Only onto
            // this conversation's view (SME-51 B11), and only the latest
            // open's: after a close and a reopen, an older fetch landing
            // late would overwrite the newer one's result.
            let detail = get_context_detail(id).await.map_err(|e| server_error_message(&e));
            if selected() == Some(id) && *detail_request.peek() == request {
                context_detail.set(Some(detail));
            }
        });
    };

    rsx! {
        if let Some(snapshot) = context_usage() {
            button {
                r#type: "button",
                class: "context-usage-bar",
                onmounted: move |evt| context_bar_el.set(Some(evt)),
                onclick: move |_| open_context_detail(),
                if let Some(percent) = context_usage_percent(&snapshot) {
                    div { class: "context-usage-track",
                        div {
                            class: "context-usage-fill",
                            style: "width: {percent}%",
                        }
                    }
                    span { class: "context-usage-label", "{percent}% of context" }
                } else {
                    span { class: "context-usage-label", "No usage yet" }
                }
            }
        } else {
            // The bar's space, kept until the usage arrives: it
            // always shows once it has, and appearing above the
            // transcript would push it down under the pointer
            // (SME-75).
            div { class: "context-usage-bar context-usage-bar-pending", aria_hidden: "true",
                span { class: "context-usage-label", "\u{a0}" }
            }
        }
        if context_detail_open() {
            div { class: "context-detail-overlay",
                onclick: move |_| close_context_detail(),
                onkeydown: move |e: Event<KeyboardData>| {
                    if e.data().key() == keyboard_types::Key::Escape {
                        close_context_detail();
                    }
                },
                // The view is modal, so Tab cycles inside it:
                // focus reaching either sentinel wraps around
                // to the view's other end.
                div {
                    class: "focus-sentinel",
                    tabindex: "0",
                    onfocus: move |_| {
                        spawn(async move {
                            let _ = document::eval(FOCUS_LAST_IN_CONTEXT_DETAIL).await;
                        });
                    },
                }
                div {
                    class: "context-detail-panel",
                    role: "dialog",
                    aria_modal: "true",
                    aria_label: "Context",
                    // Focusable (not in the tab order), so a
                    // click on the view's text keeps focus in
                    // it and Escape still reaches the overlay.
                    tabindex: "-1",
                    onclick: move |evt| evt.stop_propagation(),
                    button {
                        r#type: "button",
                        class: "context-detail-close",
                        aria_label: "Close",
                        onmounted: move |evt: MountedEvent| async move {
                            let _ = evt.set_focus(true).await;
                        },
                        onclick: move |_| close_context_detail(),
                        "×"
                    }
                    {render_context_detail_body(context_detail().as_ref())}
                }
                div {
                    class: "focus-sentinel",
                    tabindex: "0",
                    onfocus: move |_| {
                        spawn(async move {
                            let _ = document::eval(FOCUS_FIRST_IN_CONTEXT_DETAIL).await;
                        });
                    },
                }
            }
        }
    }
}
