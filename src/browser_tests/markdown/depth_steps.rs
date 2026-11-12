//! The per-depth driver of `scenario_markdown_deep_code` (SME-500): it runs
//! one depth's body under the depth's time limit and turns how it ended into
//! that depth's failures.

use std::future::Future;
use std::time::Duration;

/// Runs one depth's `body` under `limit`. A depth that hangs or panics ends
/// alone, so the rest still run; its failures come back prefixed with the
/// depth.
pub(super) async fn run_depth(depth: usize, limit: Duration, body: impl Future<Output = Vec<String>>) -> Vec<String> {
    let outcome =
        futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(tokio::time::timeout(limit, body))).await;
    let problems = match outcome {
        Ok(Ok(problems)) => problems,
        Ok(Err(_)) => vec![format!("didn't finish within {} s", limit.as_secs())],
        Err(panic) => vec![panic.downcast_ref::<String>().cloned().unwrap_or_else(|| "panicked".to_string())],
    };
    problems.into_iter().map(|p| format!("depth {depth}: {p}")).collect()
}
