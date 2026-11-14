//! The Claude Code provider kind's tests. None runs the real CLI or
//! reaches Anthropic: the status tests run a fake `claude` shell script,
//! and the transport's run the test binary itself as the CLI (`fake`).

use super::*;

pub(crate) mod bridge;
mod close;
mod compaction;
mod errors;
pub(crate) mod fake;
mod gate;
mod render;
mod status;
pub(crate) mod usage;

/// Q6 (SME-523): every launch pins the prompt cache's TTL at an hour, the
/// lifetime the re-seed's timing assumes (`turn::reseed::CACHE_TTL`).
#[test]
fn test_every_launch_pins_the_prompt_cache_ttl_at_an_hour() {
    let env = process::turn_env(Vec::new(), 1_000);
    let ttl: Vec<_> = env.iter().filter(|(name, _)| name == "CLAUDE_CODE_PROMPT_CACHE_TTL").map(|(_, v)| v.clone()).collect();
    assert_eq!(ttl, vec![std::ffi::OsString::from("1h")], "{env:?}");
}
