//! The startup sweep: no start survives a restart, and nothing wakes a
//! conversation for what was queued to it before one.

use super::*;

/// What a start cut off by a restart says.
pub(crate) const RESTARTED: &str = "smelt restarted during the start";

/// Fails every child still `starting` (its start ran in a task the restart
/// ended), as `fail_start` does, and cleans up after it as a start's exit
/// would; then wakes every conversation with an undelivered report or
/// notice that isn't a `stopped` agent. The pause after a user's Stop is in
/// memory and lost too, so a conversation the user had stopped is woken
/// (SME-238 stores it). Returns the wakes' tasks.
pub(crate) async fn startup_sweep(pool: &PgPool) -> Vec<tokio::task::JoinHandle<()>> {
    let mut wakes = Vec::new();
    match db::starting_agents(pool).await {
        Ok(starting) => {
            for (child, parent) in starting {
                fail_start(pool, child, parent, RESTARTED).await;
                end_start(pool, child, false).await;
            }
        }
        Err(e) => tracing::warn!(error = %e, "couldn't list the agents a restart cut off"),
    }
    match db::conversations_with_queued_messages(pool).await {
        Ok(ids) => {
            for id in ids {
                wakes.push(crate::turn::notify(pool, id, vec![crate::turn::Notice::Wake]));
            }
        }
        Err(e) => tracing::warn!(error = %e, "couldn't list conversations with queued agent messages"),
    }
    wakes
}
