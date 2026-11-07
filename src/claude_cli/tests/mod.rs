//! The Claude Code provider kind's tests. None runs the real CLI or
//! reaches Anthropic: the status tests run a fake `claude` shell script,
//! and the transport's run the test binary itself as the CLI (`fake`).

use super::*;

pub(crate) mod bridge;
mod compaction;
mod errors;
pub(crate) mod fake;
mod render;
mod status;
