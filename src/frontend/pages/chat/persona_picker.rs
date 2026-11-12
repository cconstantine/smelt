//! "Start as a persona" (SME-481): on a new conversation's start screen,
//! under "Work on a repo", the personas of its trusted checkouts, set once
//! before the first message (`POST /api/conversations/{id}/persona`).

use super::*;
use crate::api::personas::{PersonaOption, TopLevelPersona, get_top_level_persona, set_top_level_persona};

/// The picker. Refetches on `refresh` (a `ConversationsChanged`: a set in
/// another tab) and on the conversation's repos changing (a clone finishing
/// shows its personas). Keyed by conversation where it's used, so a switch
/// starts it afresh. The set's result is read back through the refetch it
/// publishes, and also shown at once from its answer.
#[component]
pub(super) fn PersonaPicker(conversation_id: i64, state: Store<ConversationState>, refresh: Signal<u64>) -> Element {
    let repos = state.repos();
    let mut local_refresh = use_signal(|| 0u64);
    let listing = use_resource(move || async move {
        refresh();
        local_refresh();
        // Subscribed so a clone finishing refetches; the ids are enough.
        let _ = repos.read().iter().map(|r| (r.id, r.status.clone())).collect::<Vec<_>>();
        get_top_level_persona(conversation_id).await
    });
    let mut chosen: Signal<String> = use_signal(String::new);
    let mut error: Signal<Option<String>> = use_signal(|| None);
    let mut setting = use_signal(|| false);
    let set = move |_| {
        let persona = chosen.peek().clone();
        if persona.is_empty() {
            return;
        }
        setting.set(true);
        spawn(async move {
            let result = set_top_level_persona(conversation_id, persona).await;
            setting.set(false);
            match result {
                Ok(_) => {
                    error.set(None);
                    *local_refresh.write() += 1;
                }
                Err(e) => error.set(Some(server_error_message(&e))),
            }
        });
    };
    let shown: TopLevelPersona = match listing() {
        Some(Ok(listed)) => listed,
        // A listing that failed (the sandbox unreachable, say) says so
        // rather than showing nothing (review 1, L4).
        Some(Err(e)) => {
            let message = format!("Couldn't list this conversation's personas: {}", server_error_message(&e));
            return rsx! { ErrorText { message: Some(message) } };
        }
        None => return rsx! {},
    };
    if let Some(set) = &shown.set {
        let on = match (&set.model, &set.provider) {
            (Some(model), Some(provider)) => format!(" ({model} on {provider})"),
            _ => String::new(),
        };
        return rsx! {
            p { class: "persona-picker-set", id: "persona-set",
                "This conversation runs as "
                code { "{set.label}" }
                "{on}"
                if !set.applied { span { class: "muted", " \u{2014} still being applied" } }
            }
        };
    }
    // Started, or an agent: nothing to offer.
    if shown.can_set.is_some() {
        return rsx! {};
    }
    if shown.options.is_empty() {
        return rsx! {
            if !shown.notes.is_empty() {
                p { class: "muted persona-picker-note",
                    "Start as a persona: attach a trusted repo with .agents/personas/."
                    for note in shown.notes.iter() { br {} "{note}" }
                }
            }
        };
    }
    let description = shown
        .options
        .iter()
        .find(|o| o.label == chosen())
        .and_then(|o: &PersonaOption| o.description.clone());
    rsx! {
        div { class: "persona-picker",
            label { r#for: "persona-picker", "Start as a persona" }
            div { class: "persona-picker-fields",
                select {
                    id: "persona-picker",
                    value: "{chosen}",
                    onchange: move |e| chosen.set(e.value()),
                    option { value: "", "No persona" }
                    for option in shown.options.iter() {
                        option {
                            key: "{option.label}",
                            value: "{option.label}",
                            disabled: option.error.is_some(),
                            title: option.error.clone().unwrap_or_default(),
                            "{option.label}"
                        }
                    }
                }
                button {
                    r#type: "button",
                    id: "persona-picker-set",
                    disabled: chosen().is_empty() || setting(),
                    onclick: set,
                    if setting() { "Setting\u{2026}" } else { "Start as this persona" }
                }
            }
            if let Some(description) = description {
                p { class: "persona-picker-description", "{description}" }
            }
            ErrorText { message: error() }
        }
    }
}
