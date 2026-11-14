//! The Claude subscription's usage windows (SME-486): every API call the
//! CLI makes streams a `rate_limit_event` whose
//! `rate_limit_info.unifiedWindows` says, per window (`five_hour`,
//! `seven_day`, …), how much is used and when it resets. smelt keeps the
//! latest reading per provider and window (`provider_usage_windows`) and
//! shows it, so a PM can slow down before the cap.
//!
//! The flow: `run::read`'s `note` and `once`'s reader call `seen` with
//! each event, which parses it and holds the reading per conversation
//! (the CLI run knows no provider). The turn's caller, which does, then
//! calls `record` after the call, whether it ended Ok or Err, which takes
//! the reading and writes it.
//!
//! **Only numbers leave the parser.** It reads four fields per window and
//! checks each: a name of `[a-z0-9_]{1,32}`, a finite utilization from 0
//! to 10, a status from a fixed list (else `unknown`) and a reset time in
//! 2020..2100. The event's session id, uuids and every other string are
//! never kept, stored, served or logged.
//!
//! The wire types here cross to the browser; the rest is server-only.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One window of a provider's subscription usage, as served.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct UsageWindow {
    /// `five_hour`, `seven_day`, or another window the CLI named.
    pub name: String,
    /// The share used: 0.45 is 45%. Above 1.0 with overage; the parser
    /// keeps 0 to 10.
    pub utilization: f64,
    /// `allowed`, `allowed_warning`, `rejected` or `unknown`.
    pub status: String,
    pub resets_at: Option<DateTime<Utc>>,
    /// When smelt read it.
    pub seen_at: DateTime<Utc>,
    /// The reset is past: the utilization is an ended window's.
    pub reset_passed: bool,
    /// The share of the window gone by, for a window of known length.
    pub elapsed_fraction: Option<f64>,
    /// The utilization at this rate by the reset, once a seventh of the
    /// window has gone by.
    pub projected: Option<f64>,
}

/// A provider's latest readings, one per window.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ProviderUsage {
    pub provider_id: i64,
    pub windows: Vec<UsageWindow>,
    /// The server's clock when it answered: what the UI's "resets at"
    /// and "seen ago" count from.
    pub observed_at: DateTime<Utc>,
}

#[cfg(feature = "server")]
pub use server::*;

#[cfg(feature = "server")]
mod server {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use chrono::{DateTime, Duration, TimeZone, Utc};
    use serde::Serialize;
    use serde_json::Value;

    use super::{ProviderUsage, UsageWindow};

    /// The statuses the CLI sends; anything else is stored as `unknown`.
    const STATUSES: [&str; 3] = ["allowed", "allowed_warning", "rejected"];
    /// 2020-01-01 and 2100-01-01, in Unix seconds.
    const RESETS_FROM: i64 = 1_577_836_800;
    const RESETS_TO: i64 = 4_102_444_800;
    /// The most a utilization can be and still be believed.
    const MOST_UTILIZATION: f64 = 10.0;

    /// One window of one event, checked.
    #[derive(Clone, Debug, Serialize, PartialEq)]
    pub struct Window {
        pub name: String,
        pub utilization: f64,
        pub status: String,
        pub resets_at: Option<DateTime<Utc>>,
    }

    /// One event's windows, and when smelt read it.
    #[derive(Clone, Debug, Serialize, PartialEq)]
    pub struct Reading {
        pub windows: Vec<Window>,
        pub seen_at: DateTime<Utc>,
    }

    fn valid_name(name: &str) -> bool {
        (1..=32).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    }

    fn window(name: &str, fields: &Value) -> Option<Window> {
        if !valid_name(name) {
            return None;
        }
        let utilization = fields["utilization"].as_f64().filter(|u| u.is_finite() && (0.0..=MOST_UTILIZATION).contains(u))?;
        let status = fields["status"].as_str().filter(|s| STATUSES.contains(s)).unwrap_or("unknown");
        let resets_at = fields["resetsAt"]
            .as_i64()
            .filter(|s| (RESETS_FROM..=RESETS_TO).contains(s))
            .and_then(|s| Utc.timestamp_opt(s, 0).single());
        Some(Window { name: name.to_string(), utilization, status: status.to_string(), resets_at })
    }

    impl Reading {
        /// A `rate_limit_event`'s valid windows, or `None` when it has
        /// none: the last reading then stands.
        pub fn from_event(event: &Value, seen_at: DateTime<Utc>) -> Option<Self> {
            let windows: Vec<Window> = event["rate_limit_info"]["unifiedWindows"]
                .as_object()?
                .iter()
                .filter_map(|(name, fields)| window(name, fields))
                .collect();
            (!windows.is_empty()).then_some(Self { windows, seen_at })
        }
    }

    fn pending() -> std::sync::MutexGuard<'static, HashMap<i64, Reading>> {
        static PENDING: std::sync::LazyLock<Mutex<HashMap<i64, Reading>>> = std::sync::LazyLock::new(Mutex::default);
        // Every change is one insert or remove: nothing is left half-done.
        PENDING.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Holds `event`'s reading for `conversation_id` until its caller
    /// records it, replacing an older one. Read now, so a reading kept
    /// past a Stop still carries its own time.
    pub fn seen(conversation_id: i64, event: &Value) {
        if let Some(reading) = Reading::from_event(event, Utc::now()) {
            // The usage gate (SME-523) sees it as it streams.
            crate::claude_cli::gate::observe(&reading);
            hold(conversation_id, reading);
        }
    }

    /// How long a reading may stay held: one no call took (a Stop, a run
    /// settled in the background) goes when the next is held after this.
    pub const HELD_AT_MOST: Duration = Duration::hours(1);

    /// Holds `reading` for `conversation_id`, sweeping out every reading
    /// held longer than `HELD_AT_MOST`, so the map keeps none for good.
    pub fn hold(conversation_id: i64, reading: Reading) {
        let oldest = Utc::now() - HELD_AT_MOST;
        let mut held = pending();
        held.retain(|_, r| r.seen_at >= oldest);
        held.insert(conversation_id, reading);
    }

    /// Drops `conversation_id`'s held reading: a Claude Code call does so
    /// as it starts, so a leftover from an earlier call is never recorded
    /// as this call's provider's (round 1 A-L1).
    pub fn discard(conversation_id: i64) {
        pending().remove(&conversation_id);
    }

    /// `conversation_id`'s held reading, taken.
    pub fn take(conversation_id: i64) -> Option<Reading> {
        pending().remove(&conversation_id)
    }

    /// Writes `conversation_id`'s held reading, if any, as `provider_id`'s.
    /// A failed write is logged and dropped: a reading never fails a turn.
    pub async fn record(pool: &sqlx::PgPool, conversation_id: i64, provider_id: i64) {
        let Some(reading) = take(conversation_id) else {
            return;
        };
        let rows: Vec<crate::db::ProviderUsageWindowRow> = reading
            .windows
            .into_iter()
            .map(|w| crate::db::ProviderUsageWindowRow {
                window_name: w.name,
                utilization: w.utilization,
                status: w.status,
                resets_at: w.resets_at,
                seen_at: reading.seen_at,
            })
            .collect();
        if let Err(e) = crate::db::record_usage_windows(pool, provider_id, &rows).await {
            tracing::warn!(provider_id, "couldn't record the Claude subscription's usage windows: {e}");
        }
    }

    /// A window's length, for the windows whose length is known.
    fn length(name: &str) -> Option<Duration> {
        match name {
            "five_hour" => Some(Duration::hours(5)),
            "seven_day" => Some(Duration::days(7)),
            _ => None,
        }
    }

    impl UsageWindow {
        /// A stored window: whether its reset has passed as of `now`, and
        /// its pace as of `seen_at`, when its utilization was read (round 1
        /// A-L3), so the UI and `smelt-api usage` share one pace.
        pub fn from_stored(
            name: String,
            utilization: f64,
            status: String,
            resets_at: Option<DateTime<Utc>>,
            seen_at: DateTime<Utc>,
            now: DateTime<Utc>,
        ) -> Self {
            let reset_passed = resets_at.is_some_and(|at| at < now);
            let elapsed_fraction = resets_at.zip(length(&name)).map(|(at, length)| {
                let left = (at - seen_at).num_milliseconds() as f64 / length.num_milliseconds() as f64;
                (1.0 - left).clamp(0.0, 1.0)
            });
            let projected = elapsed_fraction
                .filter(|elapsed| !reset_passed && *elapsed >= 1.0 / 7.0)
                .map(|elapsed| utilization / elapsed);
            Self { name, utilization, status, resets_at, seen_at, reset_passed, elapsed_fraction, projected }
        }
    }

    impl ProviderUsage {
        /// `provider_id`'s stored rows, as of `now`.
        pub fn from_rows(provider_id: i64, rows: Vec<crate::db::ProviderUsageWindowRow>, now: DateTime<Utc>) -> Self {
            let windows = rows
                .into_iter()
                .map(|r| UsageWindow::from_stored(r.window_name, r.utilization, r.status, r.resets_at, r.seen_at, now))
                .collect();
            Self { provider_id, windows, observed_at: now }
        }
    }
}
