//! The `/secrets` settings page (SME-242): the user's named secrets, the
//! things a persona lists by name in `smelt.credentials` to hand one to
//! the agent that runs under it. Write-only by design (Answer 2): a
//! value is typed in and never shown again beyond `shown`'s capped
//! prefix — changing one means typing the new value, and there is no
//! reveal control, because there is no reveal.

use dioxus::prelude::*;

use crate::api::secrets::{
    delete_secret, list_secrets, save_secret, validate_secret_name, validate_secret_value, SecretSummary,
};
use crate::frontend::Route;

/// The page itself: list, add, delete. One load, then the signals mirror
/// it — the pattern the other settings pages share (`pages/git.rs`).
#[component]
pub fn SecretsPage() -> Element {
    let initial = use_resource(list_secrets);
    let mut loaded = use_signal(|| false);
    let mut load_error: Signal<Option<String>> = use_signal(|| None);
    let mut secrets: Signal<Vec<SecretSummary>> = use_signal(Vec::new);
    let action_error: Signal<Option<String>> = use_signal(|| None);

    use_effect(move || {
        if let Some(result) = initial() {
            match result {
                Ok(list) => secrets.set(list),
                Err(e) => load_error.set(Some(super::server_error_message(&e))),
            }
            loaded.set(true);
        }
    });

    rsx! {
        div { class: "sandbox-volumes-page secrets-page",
            div { class: "sandbox-volumes-header",
                Link { to: Route::Home {}, class: "sandbox-volumes-back-link", "\u{2190} Back to conversations" }
                h1 { "Secrets" }
                p { class: "muted",
                    "Named secrets personas hand to the agents that run under them. A persona lists a name in \
                     its `smelt.credentials`; the agent's sandbox gets a file at /etc/smelt/secrets/<name> \
                     (mode 600). Values are never shown again after you add one."
                }
            }
            super::ErrorText { message: load_error() }
            if !loaded() {
                p { class: "muted", "Loading..." }
            } else {
                SecretAddForm { secrets, action_error }
                SecretsList { secrets, action_error }
                p { class: "muted secrets-note",
                    "`git-ssh` is built in: a persona lists it without storing anything, and gets it only \
                     down a chain that has it. A named secret goes to whoever runs under a persona that \
                     lists it, whoever its ancestors are — the persona file is the whole grant. Only a \
                     short prefix of a stored value is ever shown."
                }
            }
        }
    }
}

/// Add a secret. The form checks with the server's own validators before
/// submitting, so a name that can't be a pod file name is said the same
/// way whether the browser caught it or the server did.
#[component]
fn SecretAddForm(secrets: Signal<Vec<SecretSummary>>, action_error: Signal<Option<String>>) -> Element {
    let mut secrets = secrets;
    let mut action_error = action_error;
    let mut name: Signal<String> = use_signal(String::new);
    let mut value: Signal<String> = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let add = move |evt: Event<FormData>| {
        evt.prevent_default();
        if let Some(e) = pre_submit_error(&name(), &value()) {
            action_error.set(Some(e));
            return;
        }
        let (n, v) = (name(), value());
        busy.set(true);
        spawn(async move {
            match save_secret(None, n, v).await {
                Ok(created) => {
                    secrets.write().push(created);
                    name.set(String::new());
                    value.set(String::new());
                    action_error.set(None);
                }
                Err(e) => action_error.set(Some(super::server_error_message(&e))),
            }
            busy.set(false);
        });
    };

    rsx! {
        section { class: "git-section",
            h2 { "Add a secret" }
            form { class: "sandbox-volume-add-form secrets-add-form", onsubmit: add,
                label { r#for: "secret-name", "Name" }
                input {
                    id: "secret-name",
                    r#type: "text",
                    value: "{name}",
                    placeholder: "linear-api-key",
                    oninput: move |e| { name.set(e.value()); action_error.set(None); },
                }
                label { r#for: "secret-value", "Value" }
                input {
                    id: "secret-value",
                    r#type: "password",
                    value: "{value}",
                    autocomplete: "new-password",
                    oninput: move |e| { value.set(e.value()); action_error.set(None); },
                }
                button { r#type: "submit", disabled: busy(), if busy() { "Adding..." } else { "Add" } }
            }
        }
    }
}

/// The stored secrets: name, the display prefix the server computed
/// (never re-derived here), age, and the row's two controls — Change,
/// which types a new value in place (the name is immutable, Trade-off
/// 7, so the request carries the row's own name; renaming is delete
/// and add), and Delete.
#[component]
fn SecretsList(secrets: Signal<Vec<SecretSummary>>, action_error: Signal<Option<String>>) -> Element {
    let mut secrets = secrets;
    let mut action_error = action_error;
    let mut pending_delete: Signal<Option<i64>> = use_signal(|| None);
    let mut pending_change: Signal<Option<i64>> = use_signal(|| None);
    let mut change_value: Signal<String> = use_signal(String::new);
    let confirm_delete = move |id: i64| {
        spawn(async move {
            match delete_secret(id).await {
                Ok(()) => {
                    secrets.write().retain(|s| s.id != id);
                    pending_delete.set(None);
                    action_error.set(None);
                }
                Err(e) => action_error.set(Some(super::server_error_message(&e))),
            }
        });
    };
    // The change path sends the row's existing name — read from the
    // row itself at click time: the API demands a name on the id path
    // and keeps the row's own (renaming is delete and add, Trade-off
    // 7), so the request carries what the row is called and only the
    // value changes. The value passes the server's own rule before
    // submitting, the same courtesy the add form pays.
    let confirm_change = move |id: i64| {
        spawn(async move {
            let value = change_value();
            if let Some(e) = validate_secret_value(&value).err() {
                action_error.set(Some(e));
                return;
            }
            let Some(name) = secrets.read().iter().find(|s| s.id == id).map(|s| s.name.clone()) else {
                return;
            };
            match save_secret(Some(id), name, value).await {
                Ok(updated) => {
                    if let Some(row) = secrets.write().iter_mut().find(|s| s.id == id) {
                        *row = updated;
                    }
                    pending_change.set(None);
                    change_value.set(String::new());
                    action_error.set(None);
                }
                Err(e) => action_error.set(Some(super::server_error_message(&e))),
            }
        });
    };

    rsx! {
        section { class: "git-section",
            h2 { "Stored secrets" }
            if secrets().is_empty() {
                p { class: "muted", "None yet. A persona naming a secret the store doesn't have gets nothing \
                     until you add it here — no restart needed." }
            } else {
                div { class: "sandbox-volume-list",
                    for secret in secrets() {
                        div { key: "{secret.id}", "data-secret-id": "{secret.id}", class: "sandbox-volume-row secret-row", title: secret_row(&secret),
                            div { class: "sandbox-volume-summary",
                                span { class: "sandbox-volume-name", "{secret.name}" }
                                span { class: "sandbox-volume-path secret-shown", "{secret.shown}" }
                                span { class: "secret-lines", "{lines_shown(secret.lines)}" }
                                span { class: "secret-created", "{secret.created_at.format(\"%Y-%m-%d\")}" }
                            }
                            div { class: "git-key-actions",
                                if pending_change() == Some(secret.id) {
                                    input {
                                        class: "secret-change-input",
                                        r#type: "password",
                                        value: "{change_value}",
                                        placeholder: "new value",
                                        autocomplete: "new-password",
                                        oninput: move |e| { change_value.set(e.value()); action_error.set(None); },
                                    }
                                    button {
                                        class: "secret-change-confirm",
                                        onclick: move |_| confirm_change(secret.id),
                                        "Save"
                                    }
                                } else {
                                    button {
                                        class: "secret-change",
                                        onclick: move |_| {
                                            pending_change.set(Some(secret.id));
                                            change_value.set(String::new());
                                            action_error.set(None);
                                        },
                                        "Change"
                                    }
                                }
                                if pending_delete() == Some(secret.id) {
                                    button {
                                        class: "secret-delete-confirm",
                                        onclick: move |_| confirm_delete(secret.id),
                                        "Delete {secret.name}"
                                    }
                                } else {
                                    button {
                                        class: "secret-delete",
                                        onclick: move |_| pending_delete.set(Some(secret.id)),
                                        "Delete"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The form's pre-submit check, in the server's words: an empty value
/// and a name that isn't a pod file name are refused before the request,
/// said exactly as the server would say them.
fn pre_submit_error(name: &str, value: &str) -> Option<String> {
    validate_secret_name(name).err().or_else(|| validate_secret_value(value).err())
}

/// A row's title text: the full name — never truncated, however long —
/// then the server's display (`shown` rendered verbatim, never
/// recomputed here) with its line count beside it (SME-334) and the
/// age.
fn secret_row(secret: &SecretSummary) -> String {
    format!(
        "{} — {} · {} — added {}",
        secret.name,
        secret.shown,
        lines_shown(secret.lines),
        secret.created_at.format("%Y-%m-%d")
    )
}

/// The row's line-count display (SME-334): the number and the word
/// agreeing — `1 line`, `47 lines`. A count, like `shown`'s byte
/// length: never a part of the value.
fn lines_shown(lines: usize) -> String {
    format!("{lines} {}", if lines == 1 { "line" } else { "lines" })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T11: the row's own label — a 64-character name in full, and the
    /// `shown` from an 8-byte value exactly as the server sent it (the
    /// client renders it, never recomputes a prefix; the arithmetic is
    /// pinned by the api tests' T14).
    #[test]
    fn test_a_row_shows_its_long_name_and_the_servers_display() {
        let name = format!("linear-api-key-{}", "x".repeat(49));
        assert_eq!(name.len(), 64, "the CHECK's widest name");
        let eight = "abcdefgh";
        // What the server's `shown` computes for an 8-byte value — at
        // least 8 hidden means none of it shows. The row renders it
        // verbatim.
        let one_line = SecretSummary {
            id: 1,
            name: name.clone(),
            created_at: chrono::NaiveDate::from_ymd_opt(2026, 10, 30).expect("a date").and_time(chrono::NaiveTime::MIN),
            updated_at: chrono::NaiveDate::from_ymd_opt(2026, 10, 30).expect("a date").and_time(chrono::NaiveTime::MIN),
            shown: "(8 bytes)".to_string(),
            lines: 1,
        };
        let row = secret_row(&one_line);
        assert!(row.contains(&name), "the name is not truncated: {row}");
        assert!(row.contains("(8 bytes)"), "the server's display verbatim: {row}");
        assert!(row.contains("1 line"), "the row's title carries the line count, singular: {row}");
        assert!(!row.contains(eight), "no value characters reached the row: {row}");
        // The plural, for anything but one (a 47-line kubeconfig).
        let many = SecretSummary { lines: 47, ..one_line.clone() };
        let row_many = secret_row(&many);
        assert!(row_many.contains("47 lines"), "the count reads as a count: {row_many}");
    }

    /// T11: the pre-submit check says the server's words — empty value,
    /// bad name, and the built-in's name — and stays silent about a
    /// pair the server would take.
    #[test]
    fn test_the_form_says_the_servers_words() {
        assert_eq!(pre_submit_error("", "v").as_deref(), validate_secret_name("").err().as_deref());
        assert_eq!(pre_submit_error("bad name", "v").as_deref(), validate_secret_name("bad name").err().as_deref());
        assert_eq!(
            pre_submit_error("git-ssh", "v").as_deref(),
            Some("A secret can't be named git-ssh — that name is smelt's built-in git key.")
        );
        assert_eq!(pre_submit_error("linear-key", "").as_deref(), validate_secret_value("").err().as_deref());
        assert!(pre_submit_error("linear-key", "secret-value").is_none());
        // SME-334: a multi-line value is the server's business — and the
        // server takes it, byte-exact. The form must not grow its own
        // newline-aversion to out-early the server's rule.
        assert!(pre_submit_error("k", "a\nb").is_none());
    }
}
