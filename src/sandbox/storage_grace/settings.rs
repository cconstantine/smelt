//! The storage-grace settings (SME-345, SME-521): three whole-day graces,
//! each read once from the environment and parsed by one rule.

use super::*;

/// `SANDBOX_STORAGE_REMOVAL_DAYS`, default 7, `0` = automatic removal
/// off for every conversation (the pass's off switch); read once when the
/// pass task starts, like `idle_stop_minutes` at the first call. Changing
/// it needs a restart.
pub(crate) fn storage_removal_days() -> u32 {
    static DAYS: OnceLock<u32> = OnceLock::new();
    *DAYS.get_or_init(|| storage_removal_days_from(std::env::var("SANDBOX_STORAGE_REMOVAL_DAYS").ok()))
}

/// `SANDBOX_AGENT_STORAGE_REMOVAL_DAYS`, default 1: the grace of a
/// sub-agent that finished, was stopped or failed (SME-521). `0` keeps
/// such storage until it's removed by hand.
pub(crate) fn agent_storage_removal_days() -> u32 {
    static DAYS: OnceLock<u32> = OnceLock::new();
    *DAYS.get_or_init(|| {
        days_from(AGENT_DAYS_NAME, std::env::var(AGENT_DAYS_NAME).ok(), DEFAULT_AGENT_STORAGE_REMOVAL_DAYS)
    })
}

/// `SANDBOX_USER_STOPPED_STORAGE_DAYS`, default 30: the grace of storage
/// the user's Stop left, and of a stop from before SME-521 whose cause
/// can't be told. `0` keeps a Stop's storage until it's removed by hand.
pub(crate) fn user_stopped_storage_days() -> u32 {
    static DAYS: OnceLock<u32> = OnceLock::new();
    *DAYS.get_or_init(|| {
        days_from(USER_DAYS_NAME, std::env::var(USER_DAYS_NAME).ok(), DEFAULT_USER_STOPPED_STORAGE_DAYS)
    })
}

/// The three graces together, as the rule and the pages take them.
pub(crate) fn grace_settings() -> GraceSettings {
    GraceSettings {
        default_days: storage_removal_days(),
        agent_days: agent_storage_removal_days(),
        user_days: user_stopped_storage_days(),
    }
}

const DEFAULT_DAYS_NAME: &str = "SANDBOX_STORAGE_REMOVAL_DAYS";
pub(in crate::sandbox) const AGENT_DAYS_NAME: &str = "SANDBOX_AGENT_STORAGE_REMOVAL_DAYS";
pub(in crate::sandbox) const USER_DAYS_NAME: &str = "SANDBOX_USER_STOPPED_STORAGE_DAYS";

/// The default grace period, a week.
const DEFAULT_SANDBOX_STORAGE_REMOVAL_DAYS: u32 = 7;
/// A finished sub-agent's grace, a day.
pub(in crate::sandbox) const DEFAULT_AGENT_STORAGE_REMOVAL_DAYS: u32 = 1;
/// A user's Stop's grace, 30 days (the user, SME-521 Q1).
pub(in crate::sandbox) const DEFAULT_USER_STOPPED_STORAGE_DAYS: u32 = 30;

/// The largest grace that means something, a year: past it, warn and use
/// the default — the ceiling is what catches a value written in seconds
/// (`604800`) and answers it with the default, not ~1,657 years.
const MAX_SANDBOX_STORAGE_REMOVAL_DAYS: i64 = 365;

/// The days for a `SANDBOX_STORAGE_REMOVAL_DAYS` of `setting`. Kept under
/// its old name so its tests read as before.
pub(in crate::sandbox) fn storage_removal_days_from(setting: Option<String>) -> u32 {
    days_from(DEFAULT_DAYS_NAME, setting, DEFAULT_SANDBOX_STORAGE_REMOVAL_DAYS)
}

/// The days for setting `name` of `setting`: unset or empty is `default`,
/// `0` means kept (off), and anything that isn't a whole number of days
/// from 0 to a year logs a warning and uses `default`. Pulled out so the
/// parsing is testable without the environment.
pub(in crate::sandbox) fn days_from(name: &str, setting: Option<String>, default: u32) -> u32 {
    let Some(setting) = setting.filter(|s| !s.is_empty()) else {
        return default;
    };
    match setting.parse::<i64>() {
        Ok(days) if (0..=MAX_SANDBOX_STORAGE_REMOVAL_DAYS).contains(&days) => u32::try_from(days).unwrap_or(default),
        _ => {
            tracing::warn!(
                setting = name,
                days = %setting,
                ceiling_days = MAX_SANDBOX_STORAGE_REMOVAL_DAYS,
                default_days = default,
                "a storage grace setting is not a whole number of days from 0 to a year; using the default instead"
            );
            default
        }
    }
}
