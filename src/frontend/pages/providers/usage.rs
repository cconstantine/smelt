//! The Claude subscription's usage windows (SME-486), as the provider
//! page, the providers list and the chat's context bar show them. Pure
//! text helpers first, then the provider page's block and the list's
//! suffix.

use chrono::{DateTime, Utc};

use super::*;
use crate::api::usage_windows::get_provider_usage;
use crate::claude_cli::usage::{ProviderUsage, UsageWindow};
use crate::frontend::timezone::{LocalZone, use_local_zone};

/// The share a window is at, as a whole percent.
pub fn usage_percent(utilization: f64) -> u32 {
    (utilization * 100.0).round().clamp(0.0, 1000.0) as u32
}

/// The percent a window shows: none once its reset has passed, as its
/// utilization is an ended window's (round 1 B-L5).
pub fn shown_percent(window: &UsageWindow) -> Option<u32> {
    (!window.reset_passed).then(|| usage_percent(window.utilization))
}

/// `45%`, or a dash once the reset has passed.
fn shown_text(window: &UsageWindow) -> String {
    shown_percent(window).map_or_else(|| "\u{2014}".to_string(), |p| format!("{p}%"))
}

/// `5h`, `7d`, or the window's own name.
fn short_name(name: &str) -> &str {
    match name {
        "five_hour" => "5h",
        "seven_day" => "7d",
        other => other,
    }
}

/// `5-hour`, `7-day`, or the window's own name.
fn long_name(name: &str) -> &str {
    match name {
        "five_hour" => "5-hour",
        "seven_day" => "7-day",
        other => other,
    }
}

/// The windows in showing order: 5-hour, 7-day, then the rest by name.
fn ordered(usage: &ProviderUsage) -> Vec<&UsageWindow> {
    let rank = |name: &str| match name {
        "five_hour" => 0,
        "seven_day" => 1,
        _ => 2,
    };
    let mut windows: Vec<&UsageWindow> = usage.windows.iter().collect();
    windows.sort_by(|a, b| rank(&a.name).cmp(&rank(&b.name)).then_with(|| a.name.cmp(&b.name)));
    windows
}

/// `5h 45% · 7d 32%`: a window whose reset has passed reads `5h —`.
/// Empty with no reading.
pub fn usage_short_text(usage: &ProviderUsage) -> String {
    ordered(usage)
        .iter()
        .map(|w| format!("{} {}", short_name(&w.name), shown_text(w)))
        .collect::<Vec<_>>()
        .join(" \u{b7} ")
}

/// `resets 14:20 PST` within a day of `now`, else `resets Mon 09:00 PST`,
/// on the viewer's wall clock with its local weekday (SME-523); `reset
/// since` once it's past.
pub fn reset_text(window: &UsageWindow, now: DateTime<Utc>, zone: &LocalZone) -> String {
    match window.resets_at {
        None => "reset time unknown".to_string(),
        Some(_) if window.reset_passed => "reset since".to_string(),
        Some(at) if at - now < chrono::Duration::hours(24) => {
            format!("resets {} {}", zone.local(at).format("%H:%M"), zone.label)
        }
        Some(at) => format!("resets {} {}", zone.local(at).format("%a %H:%M"), zone.label),
    }
}

/// `seen just now`, `seen 3 min ago`, `seen 2 h ago` or `seen 4 d ago`.
pub fn seen_text(seen_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let minutes = (now - seen_at).num_minutes().max(0);
    match minutes {
        0 => "seen just now".to_string(),
        1..=59 => format!("seen {minutes} min ago"),
        60..=1439 => format!("seen {} h ago", minutes / 60),
        _ => format!("seen {} d ago", minutes / 1440),
    }
}

/// The context bar indicator's hover text: `Claude subscription: 5-hour
/// 45%, resets 14:20 UTC; 7-day 32%, resets Mon 09:00 UTC`; an ended
/// window reads `5-hour —, reset since`.
pub fn usage_title(usage: &ProviderUsage, zone: &LocalZone) -> String {
    let parts: Vec<String> = ordered(usage)
        .iter()
        .map(|w| format!("{} {}, {}", long_name(&w.name), shown_text(w), reset_text(w, usage.observed_at, zone)))
        .collect();
    format!("Claude subscription: {}", parts.join("; "))
}

/// The indicator's colour (`subscription-usage-<level>`): `high` at 90%
/// or `rejected`, `warn` at 70%, `allowed_warning` or a 7-day pace at
/// 100%, else none. A window whose reset has passed counts for nothing.
/// It compares the whole percent shown (`usage_percent`, a half rounding
/// up), as `smelt-api usage`'s verdict does (round 1 B-L3).
pub fn usage_level(usage: &ProviderUsage) -> Option<&'static str> {
    let live: Vec<&UsageWindow> = usage.windows.iter().filter(|w| !w.reset_passed).collect();
    if live.iter().any(|w| usage_percent(w.utilization) >= 90 || w.status == "rejected") {
        return Some("high");
    }
    let pace = |w: &&UsageWindow| w.name == "seven_day" && w.projected.is_some_and(|p| usage_percent(p) >= 100);
    if live.iter().any(|w| usage_percent(w.utilization) >= 70 || w.status == "allowed_warning" || pace(w)) {
        return Some("warn");
    }
    None
}

/// A Claude Code provider's page: each window's share, reset, when smelt
/// read it and, for the 7-day window, its pace.
#[component]
pub(super) fn SubscriptionUsage(id: i64, refresh: Signal<u64>) -> Element {
    let zone = use_local_zone();
    let usage = use_resource(move || async move {
        refresh();
        get_provider_usage(id).await
    });
    let body = match usage() {
        None => rsx! { p { class: "muted", "Loading\u{2026}" } },
        Some(Err(e)) => rsx! { super::super::ErrorText { message: Some(server_error_message(&e)) } },
        Some(Ok(usage)) if usage.windows.is_empty() => rsx! {
            p { class: "muted", "No reading yet: smelt records one with each Claude Code turn." }
        },
        Some(Ok(usage)) => rsx! {
            for w in ordered(&usage) {
                div { class: "subscription-window", key: "{w.name}",
                    span { class: "subscription-window-name", "{long_name(&w.name)}" }
                    div { class: "context-usage-track",
                        if let Some(percent) = shown_percent(w) {
                            div { class: "context-usage-fill", style: "width: {percent.min(100)}%" }
                        }
                    }
                    span { class: "subscription-window-percent", "{shown_text(w)}" }
                    span { class: "muted subscription-window-when",
                        "{reset_text(w, usage.observed_at, &zone.read())} \u{b7} {seen_text(w.seen_at, usage.observed_at)}"
                        if let Some(projected) = w.projected.filter(|_| w.name == "seven_day") {
                            " \u{b7} on pace for {usage_percent(projected)}% by reset"
                        }
                    }
                }
            }
        },
    };
    rsx! {
        section { class: "subscription-usage-section",
            h3 { "Subscription usage" }
            {body}
        }
    }
}

/// The providers list's ` · 5h 45% · 7d 32%` after a Claude Code row's
/// kind, once it has a reading.
#[component]
pub(super) fn ProviderUsageSuffix(id: i64) -> Element {
    let usage = use_resource(move || async move { get_provider_usage(id).await });
    let text = match usage() {
        Some(Ok(usage)) => usage_short_text(&usage),
        _ => String::new(),
    };
    rsx! {
        if !text.is_empty() {
            span { class: "provider-usage-suffix", " \u{b7} {text}" }
        }
    }
}
