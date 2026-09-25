//! The pods view: every live sandbox pod across all conversations, and
//! stopping one. See docs/projects/plans/pod-management.md.

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

#[cfg(feature = "server")]
use crate::db;

/// Whether a pod is doing anything. Busy while any of its terminals has a
/// command running; otherwise idle since its last sign of activity.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum PodActivity {
    Busy,
    IdleSince(NaiveDateTime),
}

/// `row`'s activity: busy while a command runs, and otherwise idle since
/// the latest of its last command finishing, its conversation's last
/// message (which stands in for file and web tool calls, which leave no
/// per-pod record), and the pod starting.
#[cfg(feature = "server")]
fn pod_activity(row: &db::LivePodRow) -> PodActivity {
    if row.running_commands > 0 {
        return PodActivity::Busy;
    }
    let latest = [Some(row.created_at), Some(row.conversation_updated_at), row.last_command_finished_at]
        .into_iter()
        .flatten()
        .max()
        .expect("created_at is always present");
    PodActivity::IdleSince(latest)
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

    fn at(hour: u32) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 9, 25)
            .expect("a valid date")
            .and_hms_opt(hour, 0, 0)
            .expect("a valid time")
    }

    fn row(created: u32, message: u32, finished: Option<u32>, running: i64) -> db::LivePodRow {
        db::LivePodRow {
            pod_id: 1,
            conversation_id: 1,
            conversation_title: "t".to_string(),
            conversation_updated_at: at(message),
            created_at: at(created),
            live_terminals: 1,
            running_commands: running,
            last_command_finished_at: finished.map(at),
        }
    }

    #[test]
    fn test_pod_activity_is_busy_while_a_command_runs() {
        assert_eq!(pod_activity(&row(1, 2, Some(3), 1)), PodActivity::Busy);
    }

    #[test]
    fn test_pod_activity_is_idle_since_the_latest_sign_of_activity() {
        assert_eq!(pod_activity(&row(1, 2, Some(3), 0)), PodActivity::IdleSince(at(3)), "last command");
        assert_eq!(pod_activity(&row(1, 4, Some(3), 0)), PodActivity::IdleSince(at(4)), "last message");
        assert_eq!(pod_activity(&row(5, 4, None, 0)), PodActivity::IdleSince(at(5)), "pod start");
    }
}
