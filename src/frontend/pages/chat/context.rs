//! The context usage meter and its detail dialog.

use super::*;

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
    if spend.calls == 0 {
        return "No completed model calls yet.".to_string();
    }
    let Some(cost) = spend.cost_usd else {
        return "No price for this model, so tokens only.".to_string();
    };
    // A cheap model's call costs a fraction of a cent: two places would
    // show it as nothing.
    let dollars = if cost > 0.0 && cost < 0.01 { format!("${cost:.4}") } else { format!("${cost:.2}") };
    match spend.unpriced_calls {
        0 => dollars,
        1 => format!("{dollars}, plus 1 call with no price"),
        n => format!("{dollars}, plus {n} calls with no price"),
    }
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

    // Fetches a fresh detail snapshot every time it's opened, rather than
    // caching — matches the idea's "most recently sent request" decision;
    // reopening after a new turn should show that turn's numbers, not a
    // stale first-open snapshot.
    let mut open_context_detail = move || {
        let Some(id) = selected() else { return };
        context_detail_open.set(true);
        // Not the last snapshot, possibly another conversation's (SME-51 B11).
        context_detail.set(None);
        spawn(async move {
            if let Ok(detail) = get_context_detail(id).await
                && selected() == Some(id)
            {
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
                    match context_detail() {
                        None => rsx! { p { "Loading…" } },
                        Some(detail) => rsx! {
                            h3 { "Context" }
                            if !detail.instructions.is_empty() {
                                h4 { class: "context-detail-heading", "Project instructions ({detail.instructions.len()})" }
                                p { class: "muted", "AGENTS.md files from this conversation's repos, sent with every turn as part of the system prompt below." }
                                for doc in &detail.instructions {
                                    details { class: "context-detail-instructions",
                                        summary {
                                            code { "{doc.path}" }
                                            span { class: "muted", " {instructions_source(doc)}" }
                                        }
                                        pre { class: "context-detail-prompt", "{doc.content}" }
                                    }
                                }
                            }
                            h4 { class: "context-detail-heading", "System prompt" }
                            if let Some(system) = &detail.system {
                                // Its own line breaks and headings, not one
                                // run-together paragraph (SME-41 D7).
                                pre { class: "context-detail-prompt", "{system}" }
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
                                    "{detail.spend.calls} model calls — uncached input: {detail.spend.input_tokens}, "
                                    "cache write: {detail.spend.cache_creation_input_tokens}, cache read: {detail.spend.cache_read_input_tokens}, "
                                    "output: {detail.spend.output_tokens}"
                                }
                                p { class: "muted",
                                    "Completed calls only: a stopped or failed call isn't counted. Each call is priced from models.dev's list "
                                    "when it finished, without time-of-day surcharges such as DeepSeek's peak hours."
                                    if let Some(at) = detail.prices_as_of {
                                        {format!(" Prices as of {} UTC.", at.format("%Y-%m-%d %H:%M"))}
                                    }
                                }
                            }
                            h4 { "Tools ({detail.tools.len()})" }
                            for tool in &detail.tools {
                                div { class: "context-detail-tool",
                                    strong { "{tool.name}" }
                                    p { "{tool.description}" }
                                    pre { "{tool.input_schema}" }
                                }
                            }
                        },
                    }
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
