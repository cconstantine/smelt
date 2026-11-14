//! The viewer's time zone (SME-523), for times shown in local time: the
//! usage windows' resets and the usage gate's hold. Read once per page
//! load from the browser; SSR, and a read that fails, are UTC.

use chrono::{DateTime, NaiveDateTime, Utc};
use dioxus::prelude::*;

/// A zone as the browser names it now: minutes ahead of UTC, and its
/// short name (`PST`, `IST`, or `UTC+5:30` where the locale has none).
#[derive(Clone, Debug, PartialEq)]
pub struct LocalZone {
    pub offset_minutes: i32,
    pub label: String,
}

impl Default for LocalZone {
    fn default() -> Self {
        Self::utc()
    }
}

impl LocalZone {
    pub fn utc() -> Self {
        Self { offset_minutes: 0, label: "UTC".to_string() }
    }

    /// `at` on this zone's wall clock.
    pub fn local(&self, at: DateTime<Utc>) -> NaiveDateTime {
        at.naive_utc() + chrono::Duration::minutes(i64::from(self.offset_minutes))
    }
}

/// The browser's zone now, or `None` when the page can't say (SSR, a
/// failed eval). The one place that reads it: `use_local_zone` and the
/// chat's `use_timezone_offset` both come here.
#[cfg(feature = "web")]
pub async fn read_local_zone() -> Option<LocalZone> {
    let value = document::eval(
        "const part = new Intl.DateTimeFormat([], {timeZoneName: 'short'}).formatToParts(new Date()).find(p => p.type === 'timeZoneName');
         return [-new Date().getTimezoneOffset(), part ? part.value : ''];",
    )
    .await
    .ok()?;
    let offset_minutes = value.get(0)?.as_i64()? as i32;
    let label = value.get(1).and_then(|l| l.as_str()).filter(|l| !l.is_empty()).map(str::to_string);
    Some(LocalZone { offset_minutes, label: label.unwrap_or_else(|| offset_label(offset_minutes)) })
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
