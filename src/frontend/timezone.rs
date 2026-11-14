//! The viewer's time zone (SME-523), for times shown in local time: the
//! usage windows' resets and the usage gate's hold. Read once per page
//! load from the browser; SSR, and a read that fails, are UTC.
//!
//! Each instant is shown with the offset and name the zone has *at that
//! instant*, not now's (PR #353 review 2 L7): a 7-day reset past a
//! daylight-saving change shows that side's wall clock and name. The
//! browser says, along with now's offset, every change of offset in the
//! next `CHANGES_AHEAD_DAYS` days, which covers any reset or hold shown.

use chrono::{DateTime, NaiveDateTime, Utc};
use dioxus::prelude::*;

/// How far ahead the browser lists the zone's changes: past the 7-day
/// window's furthest reset.
#[cfg_attr(not(feature = "web"), expect(dead_code, reason = "only the browser reads its zone"))]
const CHANGES_AHEAD_DAYS: u32 = 8;

/// A zone as the browser names it: minutes ahead of UTC and its short
/// name (`PST`, `IST`, or `UTC+5:30` where the locale has none) now, and
/// from each later change of offset on.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalZone {
    pub offset_minutes: i32,
    pub label: String,
    /// The changes ahead, in time order: from when, and the offset and
    /// name from then.
    pub changes: Vec<(DateTime<Utc>, i32, String)>,
}

impl Default for LocalZone {
    fn default() -> Self {
        Self::utc()
    }
}

impl LocalZone {
    pub fn utc() -> Self {
        Self::fixed(0, "UTC")
    }

    /// A zone with no changes ahead.
    pub fn fixed(offset_minutes: i32, label: &str) -> Self {
        Self { offset_minutes, label: label.to_string(), changes: Vec::new() }
    }

    /// The offset and name in force at `at`.
    fn at(&self, at: DateTime<Utc>) -> (i32, &str) {
        self.changes
            .iter()
            .rev()
            .find(|(from, _, _)| *from <= at)
            .map_or((self.offset_minutes, self.label.as_str()), |(_, offset, label)| (*offset, label.as_str()))
    }

    /// `at` on this zone's wall clock, at its offset then.
    pub fn local(&self, at: DateTime<Utc>) -> NaiveDateTime {
        at.naive_utc() + chrono::Duration::minutes(i64::from(self.at(at).0))
    }

    /// The zone's name at `at`.
    pub fn label_at(&self, at: DateTime<Utc>) -> &str {
        self.at(at).1
    }
}

/// The browser's zone now, or `None` when the page can't say (SSR, a
/// failed eval). The one place that reads it: `use_local_zone` and the
/// chat's `use_timezone_offset` both come here.
#[cfg(feature = "web")]
pub async fn read_local_zone() -> Option<LocalZone> {
    let script = format!(
        "const name = ms => {{ const p = new Intl.DateTimeFormat([], {{timeZoneName: 'short'}}).formatToParts(new Date(ms)).find(p => p.type === 'timeZoneName'); return p ? p.value : ''; }};
         const off = ms => -new Date(ms).getTimezoneOffset();
         const now = Date.now(), hour = 3600000, out = [[now, off(now), name(now)]];
         for (let t = now; t < now + {CHANGES_AHEAD_DAYS} * 24 * hour; t += hour) {{
           if (off(t) === off(t + hour)) continue;
           let lo = t, hi = t + hour;
           while (hi - lo > 1000) {{ const mid = Math.floor((lo + hi) / 2); if (off(mid) === off(t)) lo = mid; else hi = mid; }}
           out.push([hi, off(hi), name(hi)]);
         }}
         return out;"
    );
    let value = document::eval(&script).await.ok()?;
    let entries = value.as_array()?;
    let entry = |v: &serde_json::Value| -> Option<(DateTime<Utc>, i32, String)> {
        let at = DateTime::from_timestamp_millis(v.get(0)?.as_f64()? as i64)?;
        let offset_minutes = v.get(1)?.as_i64()? as i32;
        let label = v.get(2).and_then(|l| l.as_str()).filter(|l| !l.is_empty()).map_or_else(|| offset_label(offset_minutes), str::to_string);
        Some((at, offset_minutes, label))
    };
    let (_, offset_minutes, label) = entry(entries.first()?)?;
    let changes = entries.iter().skip(1).filter_map(entry).collect();
    Some(LocalZone { offset_minutes, label, changes })
}

/// `UTC`, `UTC+5:30` or `UTC-8`: a zone's name when the browser gives none.
#[cfg_attr(not(any(feature = "web", test)), expect(dead_code, reason = "only the browser reads its zone"))]
pub fn offset_label(offset_minutes: i32) -> String {
    let (sign, abs) = if offset_minutes < 0 { ('-', -offset_minutes) } else { ('+', offset_minutes) };
    match (abs / 60, abs % 60) {
        (0, 0) => "UTC".to_string(),
        (h, 0) => format!("UTC{sign}{h}"),
        (h, m) => format!("UTC{sign}{h}:{m:02}"),
    }
}

/// The viewer's zone: UTC until the browser has said (and for good
/// during SSR), then the browser's.
pub fn use_local_zone() -> Signal<LocalZone> {
    let zone = use_signal(LocalZone::utc);
    #[cfg(feature = "web")]
    {
        let mut zone = zone;
        use_hook(move || {
            spawn(async move {
                if let Some(read) = read_local_zone().await {
                    zone.set(read);
                }
            });
        });
    }
    zone
}
