//! The startup sweep: no start survives a restart, and nothing wakes a
//! conversation for what was queued to it before one.

use super::*;

/// What a start cut off by a restart says.
pub(crate) const RESTARTED: &str = "smelt restarted during the start";

/// The notice to a child whose turn a restart cut off (SME-238).
pub(crate) const RESTARTED_MID_TURN: &str = "smelt restarted during your turn; carry on";

/// Fails every child still `starting` (its start ran in a task the restart
/// ended), as `fail_start` does, and cleans up after it as a start's exit
/// would; then wakes every conversation with an undelivered report or
/// notice that isn't a `stopped` agent nor paused by the user (the pause
/// is stored, SME-238, and restored before this runs). Returns the wakes'
/// tasks.
pub(crate) async fn startup_sweep(pool: &PgPool) -> Vec<tokio::task::JoinHandle<()>> {
    let mut wakes = Vec::new();
    match db::starting_agents(pool).await {
        Ok(starting) => {
            for (child, parent) in starting {
                // The start failure's own wake belongs to the awaited set
                // just like the ones below: the report is queued by
                // `fail_start` itself, and the wake it hands back is what
                // runs the parent's turn to deliver it (SME-341) — awaited
                // only by its handle, never by luck of the queries below
                // listing the same row first.
                wakes.extend(fail_start(pool, child, parent, RESTARTED).await);
                end_start(pool, child, false).await;
            }
        }
        Err(e) => tracing::warn!(error = %e, "couldn't list the agents a restart cut off"),
    }
    notify_cut_off_turns(pool).await;
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

/// Queues `RESTARTED_MID_TURN` to every child a restart cut off mid-turn,
/// for the wakes after it to deliver (SME-238). Run by the startup sweep.
pub(crate) async fn notify_cut_off_turns(pool: &PgPool) {
    match db::queue_restart_notices(pool, RESTARTED_MID_TURN).await {
        Ok(told) if !told.is_empty() => tracing::info!(agents = ?told, "told agents a restart cut off their turn"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "couldn't tell the agents a restart cut off their turn"),
    }
}
