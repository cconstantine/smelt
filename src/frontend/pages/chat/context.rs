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
