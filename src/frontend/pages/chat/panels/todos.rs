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

/// What the panel says over a list nobody is working on: with no turn
/// running and items still open, the list is where the model left it, not
/// work in progress (SME-123 DR7: a finished agent's list read as if it
/// were still working). None while a turn runs or everything's done.
pub(in super::super) fn todo_note(todos: &[TodoItem], turn_running: bool) -> Option<String> {
    if turn_running {
        return None;
    }
    match todos.iter().filter(|t| t.status != TodoStatus::Completed).count() {
        0 => None,
        1 => Some("Not being worked on now: 1 still open.".to_string()),
        n => Some(format!("Not being worked on now: {n} still open.")),
    }
}

/// The model's todo list, as `todowrite` last left it.
#[component]
pub(in super::super) fn TodoPanel(
    state: Store<ConversationState>,
) -> Element {
    let todos = state.todos();
    let note = todo_note(&todos(), (state.turn_running())());
    let class = if note.is_some() { "todo-panel todo-panel-idle" } else { "todo-panel" };
    rsx! {
        aside { class,
            h3 { "Todos" }
            if let Some(note) = note {
                p { class: "todo-note", "{note}" }
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn todo(status: TodoStatus) -> TodoItem {
        TodoItem { content: "x".to_string(), status }
    }

    #[test]
    fn test_an_open_list_with_no_turn_running_says_nobody_works_on_it() {
        let list = [todo(TodoStatus::InProgress), todo(TodoStatus::Pending), todo(TodoStatus::Completed)];
        assert_eq!(todo_note(&list, false).as_deref(), Some("Not being worked on now: 2 still open."));
        assert_eq!(todo_note(&list, true), None, "a running turn is working on it");
        assert_eq!(todo_note(&[todo(TodoStatus::Completed)], false), None, "all done");
        assert_eq!(todo_note(&[todo(TodoStatus::Pending)], false).as_deref(), Some("Not being worked on now: 1 still open."));
    }
}
