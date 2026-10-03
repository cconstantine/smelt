//! The todo panel.

use super::super::*;

/// CSS class suffix for a todo's status marker — a plain string mapping,
/// not a `Display` impl, since this is presentation-only and the panel is
/// the only caller.
pub(in super::super) fn todo_status_class(status: TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "pending",
        TodoStatus::InProgress => "in-progress",
        TodoStatus::Completed => "completed",
    }
}
