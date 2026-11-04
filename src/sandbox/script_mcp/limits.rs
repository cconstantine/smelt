//! The limits on scripts' MCP calls (SME-359, Q3), per conversation and in
//! memory: a rate (a token bucket), calls in flight at once, and a budget
//! per terminal command (counted in `sandbox_mcp_calls`). A script's call
//! costs no model tokens; these keep a runaway loop from hammering a
//! provider, and the per-command summary keeps it from going unnoticed.
//! The bucket resets when smelt restarts.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

/// The deployment's limits, read once from the environment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Limits {
    /// Calls a minute; `0` turns script calls off.
    pub per_minute: u32,
    /// How many calls may go at once after a quiet spell.
    pub burst: u32,
    pub max_in_flight: u32,
    /// Calls one terminal command may make; calls traced to no command
    /// share a budget of the same size per pod.
    pub per_command: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Self { per_minute: 60, burst: 20, max_in_flight: 4, per_command: 1_000 }
    }
}

/// The variables, beside what each sets.
pub(crate) const CALLS_PER_MINUTE_ENV: &str = "SANDBOX_MCP_CALLS_PER_MINUTE";
pub(crate) const MAX_IN_FLIGHT_ENV: &str = "SANDBOX_MCP_MAX_IN_FLIGHT";
pub(crate) const CALLS_PER_COMMAND_ENV: &str = "SANDBOX_MCP_CALLS_PER_COMMAND";

impl Limits {
    /// From `lookup` (the environment, in production): an unset or
    /// unreadable variable keeps its default, warned.
    pub(crate) fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        fn read<T: std::str::FromStr>(lookup: &impl Fn(&str) -> Option<String>, name: &str, default: T) -> T {
            match lookup(name) {
                None => default,
                Some(text) => text.trim().parse().unwrap_or_else(|_| {
                    tracing::warn!(variable = name, value = %text, "not a whole number; its default is used");
                    default
                }),
            }
        }
        let default = Self::default();
        let per_minute = read(&lookup, CALLS_PER_MINUTE_ENV, default.per_minute);
        Self {
            per_minute,
            // A burst above the rate would let a minute hold more than it.
            burst: default.burst.min(per_minute.max(1)),
            max_in_flight: read(&lookup, MAX_IN_FLIGHT_ENV, default.max_in_flight).max(1),
            per_command: read(&lookup, CALLS_PER_COMMAND_ENV, default.per_command),
        }
    }

    /// The deployment's, read at the first call; a change takes a restart.
    pub(crate) fn deployment() -> Self {
        static LIMITS: LazyLock<Limits> = LazyLock::new(|| Limits::from_lookup(|name| std::env::var(name).ok()));
        *LIMITS
    }

    pub(crate) fn enabled(&self) -> bool {
        self.per_minute > 0
    }
}

/// Why a call was held back: what the script is told, and when to try
/// again, if waiting helps.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Limited {
    pub message: String,
    pub retry_after_secs: Option<u64>,
}

struct Bucket {
    tokens: f64,
    filled_at: Instant,
    in_flight: u32,
}

static BUCKETS: LazyLock<Mutex<HashMap<i64, Bucket>>> = LazyLock::new(Default::default);

/// A call's place in flight, given back when dropped.
pub(crate) struct Slot {
    conversation_id: i64,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut buckets = BUCKETS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(bucket) = buckets.get_mut(&self.conversation_id) {
            bucket.in_flight = bucket.in_flight.saturating_sub(1);
        }
    }
}

/// Takes a slot and a token for one of `conversation_id`'s calls, or says
/// which limit holds it back. A call refused here takes neither.
pub(crate) fn take(conversation_id: i64, limits: &Limits) -> Result<Slot, Limited> {
    take_at(conversation_id, limits, Instant::now())
}

fn take_at(conversation_id: i64, limits: &Limits, now: Instant) -> Result<Slot, Limited> {
    let mut buckets = BUCKETS.lock().unwrap_or_else(|e| e.into_inner());
    let bucket = buckets
        .entry(conversation_id)
        .or_insert_with(|| Bucket { tokens: f64::from(limits.burst), filled_at: now, in_flight: 0 });
    let per_second = f64::from(limits.per_minute) / 60.0;
    let elapsed = now.saturating_duration_since(bucket.filled_at).as_secs_f64();
    bucket.tokens = (bucket.tokens + elapsed * per_second).min(f64::from(limits.burst));
    bucket.filled_at = now;
    if bucket.in_flight >= limits.max_in_flight {
        return Err(Limited {
            message: format!(
                "{} of this conversation's MCP calls are already in flight, its limit ({MAX_IN_FLIGHT_ENV}); wait for one to finish",
                bucket.in_flight
            ),
            retry_after_secs: Some(1),
        });
    }
    if bucket.tokens < 1.0 {
        let wait = ((1.0 - bucket.tokens) / per_second).ceil().max(1.0) as u64;
        return Err(Limited {
            message: format!(
                "this conversation's scripts are making MCP calls faster than {} a minute, its limit ({CALLS_PER_MINUTE_ENV}); try again in {wait}s",
                limits.per_minute
            ),
            retry_after_secs: Some(wait),
        });
    }
    bucket.tokens -= 1.0;
    bucket.in_flight += 1;
    Ok(Slot { conversation_id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Distinct ids per test: the buckets are process-wide.
    fn conversation(n: i64) -> i64 {
        -359_000 - n
    }

    #[test]
    fn test_the_bucket_lets_a_burst_through_then_refills_at_the_rate() {
        let limits = Limits { per_minute: 60, burst: 2, max_in_flight: 10, per_command: 1_000 };
        let start = Instant::now();
        let id = conversation(1);
        let first = take_at(id, &limits, start).expect("first");
        let second = take_at(id, &limits, start).expect("second");
        let refused = take_at(id, &limits, start).err().expect("third is over the burst");
        assert_eq!(refused.retry_after_secs, Some(1));
        assert!(refused.message.contains(CALLS_PER_MINUTE_ENV), "{}", refused.message);
        drop((first, second));
        assert!(take_at(id, &limits, start + Duration::from_millis(1_000)).is_ok(), "a second refills one at 60/min");
    }

    #[test]
    fn test_a_call_past_the_in_flight_cap_is_refused_until_one_ends() {
        let limits = Limits { per_minute: 600, burst: 10, max_in_flight: 1, per_command: 1_000 };
        let now = Instant::now();
        let id = conversation(2);
        let held = take_at(id, &limits, now).expect("first");
        let refused = take_at(id, &limits, now).err().expect("a second while the first is in flight");
        assert!(refused.message.contains(MAX_IN_FLIGHT_ENV), "{}", refused.message);
        drop(held);
        assert!(take_at(id, &limits, now).is_ok());
    }

    #[test]
    fn test_the_limits_come_from_the_environment_with_their_defaults() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
        };
        assert_eq!(Limits::from_lookup(env(&[])), Limits::default());
        let set = Limits::from_lookup(env(&[(CALLS_PER_MINUTE_ENV, "5"), (MAX_IN_FLIGHT_ENV, "2"), (CALLS_PER_COMMAND_ENV, "30")]));
        assert_eq!(set, Limits { per_minute: 5, burst: 5, max_in_flight: 2, per_command: 30 });
        let off = Limits::from_lookup(env(&[(CALLS_PER_MINUTE_ENV, "0")]));
        assert!(!off.enabled());
        assert_eq!(Limits::from_lookup(env(&[(CALLS_PER_MINUTE_ENV, "lots")])).per_minute, 60);
    }
}
