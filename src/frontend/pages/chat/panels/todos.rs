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

/// The model's todo list, as `todowrite` last left it.
#[component]
pub(in super::super) fn TodoPanel(
    state: Store<ConversationState>,
) -> Element {
    let todos = state.todos();
    rsx! {
        aside { class: "todo-panel",
            h3 { "Todos" }
            ul { class: "todo-list",
                for (i , todo) in todos().into_iter().enumerate() {
                    li {
                        key: "{i}",
                        class: "todo-item todo-item-{todo_status_class(todo.status)}",
                        span { class: "todo-item-marker" }
                        span { class: "todo-item-content", "{todo.content}" }
                    }
                }
            }
        }
    }
}
