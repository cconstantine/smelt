//! The `SANDBOX_IDLE_NOTICE_MINUTES` default (SME-79): how long a running
//! terminal command may go untouched before smelt wakes its conversation to
//! say it's still running. Never a kill — nothing stops on this clock, and
//! this is only the deployment default: a command can ask for its own
//! interval (`idle_notice_seconds`), or for none at all.

/// `SANDBOX_IDLE_NOTICE_MINUTES`, default `5` — same pattern as
/// `default_memory_limit`, but a whole number of minutes up to a day.
///
/// It's read when a command starts and stored on its row, so changing it
/// applies to commands started after; `0` turns the check off for those, and
/// a running command keeps whatever it started with.
pub(crate) fn idle_notice_default_secs() -> i32 {
    idle_notice_secs_from(std::env::var("SANDBOX_IDLE_NOTICE_MINUTES").ok())
}

/// The seconds for a `SANDBOX_IDLE_NOTICE_MINUTES` of `setting`. Pulled
/// out of `idle_notice_default_secs` so the parsing is testable without
/// the environment. An empty setting is no setting; anything else that
/// isn't a whole number of minutes up to a day gets a warning — the
/// operator set it, and silently falling back would hide that it isn't
/// being used.
///
/// The day ceiling (SME-79 review 1) does double duty. Past it the
/// interval means "never" anyway — which `0` says honestly, so a typo'd
/// huge value is treated like the other bad ones rather than silently
/// switching the check off — and inside it `minutes * 60` can't overflow:
/// at the old size the multiply overflowed i64 for huge inputs, panicking
/// in a debug build and wrapping in release, landing on the row's
/// `CHECK (idle_notice_secs >= 0)` or on some silently different
/// interval, past both documented fallbacks.
pub(super) fn idle_notice_secs_from(setting: Option<String>) -> i32 {
    let Some(setting) = setting.filter(|s| !s.is_empty()) else {
        return DEFAULT_IDLE_NOTICE_SECS;
    };
    match setting.parse::<i64>() {
        // 0 is a choice, not a mistake: it turns the whole check off.
        Ok(0) => 0,
        Ok(minutes) if minutes > 0 && minutes <= MAX_IDLE_NOTICE_MINUTES => {
            i32::try_from(minutes * 60).unwrap_or(DEFAULT_IDLE_NOTICE_SECS)
        }
        Ok(minutes) if minutes > MAX_IDLE_NOTICE_MINUTES => {
            tracing::warn!(
                minutes,
                ceiling_minutes = MAX_IDLE_NOTICE_MINUTES,
                default_minutes = DEFAULT_IDLE_NOTICE_SECS / 60,
                "SANDBOX_IDLE_NOTICE_MINUTES is more than a day; using the default instead"
            );
            DEFAULT_IDLE_NOTICE_SECS
        }
        Ok(minutes) => {
            tracing::warn!(
                minutes,
                default_minutes = DEFAULT_IDLE_NOTICE_SECS / 60,
                "SANDBOX_IDLE_NOTICE_MINUTES is negative; using the default instead"
            );
            DEFAULT_IDLE_NOTICE_SECS
        }
        Err(_) => {
            tracing::warn!(
                minutes = %setting,
                default_minutes = DEFAULT_IDLE_NOTICE_SECS / 60,
                "SANDBOX_IDLE_NOTICE_MINUTES is not a number of minutes; using the default instead"
            );
            DEFAULT_IDLE_NOTICE_SECS
        }
    }
}

/// The largest `SANDBOX_IDLE_NOTICE_MINUTES` that means something — a day.
/// Past it, warn and use the default; see `idle_notice_secs_from`.
const MAX_IDLE_NOTICE_MINUTES: i64 = 24 * 60;

/// The default idle-notice interval, 5 minutes — see
/// `idle_notice_default_secs`.
const DEFAULT_IDLE_NOTICE_SECS: i32 = 300;
