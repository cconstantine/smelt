use dioxus::prelude::*;

use crate::api::git::{
    delete_ssh_key, forget_repo_trust, generate_ssh_key, get_git_settings, import_ssh_key,
    save_git_identity,
};
use crate::frontend::Route;
use crate::frontend::pages::chat::shown_text::hidden_text_note;
use crate::git::{GitIdentity, RepoTrustSummary, SshKeySummary};

/// The trust list's showing of a recorded key (SME-283): the key is repo
/// text the user reads at `/git`, and a row recorded before the fix can
/// be another key's invisible twin — so the label goes through SME-276's
/// escape after `remote_label`'s `file/` rewrite. `Forget` still sends
/// the raw stored key: the decision is deleted by exact match, never by
/// this showing.
fn remote_shown(decision: &RepoTrustSummary) -> String {
    crate::git::plain_multiline_text(&crate::git::remote_label(&decision.remote))
}

/// The hover notice for that showing (SME-283): the panel's pattern —
/// a one-row list has no room for a `p.muted` line — in exactly
/// `hidden_text_note`'s wording so the sinks can't drift.
fn trust_row_notice(decision: &RepoTrustSummary) -> Option<String> {
    let hidden = crate::git::hidden_characters(&decision.remote);
    (hidden > 0).then(|| hidden_text_note(hidden))
}

/// Git settings (SME-32): the commit identity and the SSH keys every
/// sandbox pod gets. A change reaches running pods straight away.
#[component]
pub fn GitSettingsPage() -> Element {
    let initial = use_resource(get_git_settings);
    let mut loaded = use_signal(|| false);
    let mut load_error: Signal<Option<String>> = use_signal(|| None);
    let mut keys: Signal<Vec<SshKeySummary>> = use_signal(Vec::new);
    let mut trust: Signal<Vec<RepoTrustSummary>> = use_signal(Vec::new);
    let mut name: Signal<String> = use_signal(String::new);
    let mut email: Signal<String> = use_signal(String::new);

    use_effect(move || {
        if let Some(result) = initial() {
            match result {
                Ok(settings) => {
                    name.set(settings.identity.name);
                    email.set(settings.identity.email);
                    keys.set(settings.keys);
                    trust.set(settings.trust);
                }
                Err(e) => load_error.set(Some(super::server_error_message(&e))),
            }
            loaded.set(true);
        }
    });

    rsx! {
        div { class: "sandbox-volumes-page git-page",
            div { class: "sandbox-volumes-header",
                Link { to: Route::Home {}, class: "sandbox-volumes-back-link", "\u{2190} Back to conversations" }
                h1 { "Git" }
                p { class: "muted",
                    "Every sandbox gets these keys and this name and email, so the model can clone and push. Changes reach running sandboxes straight away."
                }
            }
            super::ErrorText { message: load_error() }
            if !loaded() {
                p { class: "muted", "Loading..." }
            } else {
                IdentitySection { name, email }
                TrustSection { trust }
                KeysSection { keys }
            }
        }
    }
}

/// The commit identity: the name and email every sandbox's git uses.
#[component]
fn IdentitySection(name: Signal<String>, email: Signal<String>) -> Element {
    let mut name = name;
    let mut email = email;
    let mut identity_status: Signal<Option<Result<(), String>>> = use_signal(|| None);
    let save_identity = move |evt: Event<FormData>| {
        evt.prevent_default();
        let identity = GitIdentity {
            name: name().trim().to_string(),
            email: email().trim().to_string(),
        };
        spawn(async move {
            identity_status.set(Some(
                save_git_identity(identity).await.map_err(|e| super::server_error_message(&e)),
            ));
        });
    };

    rsx! {
        section { class: "git-section",
            h2 { "Commit author" }
            form { class: "sandbox-volume-add-form git-identity-form", onsubmit: save_identity,
                label { r#for: "git-identity-name", "Name" }
                input {
                    id: "git-identity-name",
                    r#type: "text",
                    value: "{name}",
                    oninput: move |e| { name.set(e.value()); identity_status.set(None); },
                }
                label { r#for: "git-identity-email", "Email" }
                input {
                    id: "git-identity-email",
                    r#type: "email",
                    value: "{email}",
                    oninput: move |e| { email.set(e.value()); identity_status.set(None); },
                }
                match identity_status() {
                    Some(Ok(())) => rsx! { p { class: "muted git-saved", "Saved." } },
                    Some(Err(err)) => rsx! { super::ErrorText { message: Some(err) } },
                    None => rsx! {},
                }
                button { r#type: "submit", "Save" }
            }
        }
    }
}

/// The repos whose AGENTS.md the user decided about, each with a way to
/// forget the decision and be asked again.
#[component]
fn TrustSection(trust: Signal<Vec<RepoTrustSummary>>) -> Element {
    let mut trust = trust;
    let mut trust_error: Signal<Option<String>> = use_signal(|| None);

    rsx! {
        section { class: "git-section",
            h2 { "Repos you've decided about" }
            p { class: "muted",
                "The model loads a repo's AGENTS.md into its instructions when it asks to; you're asked the first time for each repo. Forget a decision to be asked again."
            }
            super::ErrorText { message: trust_error() }
            if trust().is_empty() {
                p { class: "muted", "None yet. You're asked the first time the model wants to load a repo's AGENTS.md; repos you open with Work on a repo are trusted." }
            } else {
                div { class: "sandbox-volume-list",
                    for decision in trust() {
                        div { key: "{decision.remote}", class: "sandbox-volume-row",
                            div { class: "sandbox-volume-summary",
                                {
                                    let shown = remote_shown(&decision);
                                    match trust_row_notice(&decision) {
                                        Some(notice) => rsx! { span { class: "sandbox-volume-name", title: "{notice}", "{shown}" } },
                                        None => rsx! { span { class: "sandbox-volume-name", "{shown}" } },
                                    }
                                }
                                span { class: "sandbox-volume-path", if decision.trusted { "Trusted" } else { "Not trusted" } }
                            }
                            button {
                                class: "git-key-copy",
                                r#type: "button",
                                onclick: {
                                    let remote = decision.remote.clone();
                                    move |_| {
                                        let remote = remote.clone();
                                        spawn(async move {
                                            match forget_repo_trust(remote.clone()).await {
                                                Ok(()) => {
                                                    trust_error.set(None);
                                                    trust.write().retain(|t| t.remote != remote);
                                                }
                                                Err(e) => trust_error.set(Some(super::server_error_message(&e))),
                                            }
                                        });
                                    }
                                },
                                "Forget"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The SSH key every sandbox gets: listing, copying and deleting it, and
/// the form that generates or imports one while there is none.
#[component]
fn KeysSection(keys: Signal<Vec<SshKeySummary>>) -> Element {
    let mut keys = keys;
    let mut new_key_name: Signal<String> = use_signal(String::new);
    let mut private_key: Signal<String> = use_signal(String::new);
    let mut importing = use_signal(|| false);
    let mut key_error: Signal<Option<String>> = use_signal(|| None);
    let mut busy = use_signal(|| false);
    let add_key = move |evt: Event<FormData>| {
        evt.prevent_default();
        let key_name = new_key_name();
        let pasted = private_key();
        let import = importing();
        busy.set(true);
        spawn(async move {
            let result = if import {
                import_ssh_key(key_name, pasted).await
            } else {
                generate_ssh_key(key_name).await
            };
            match result {
                Ok(key) => {
                    let mut list = keys.write();
                    list.push(key);
                    list.sort_by(|a, b| a.name.cmp(&b.name));
                    new_key_name.set(String::new());
                    private_key.set(String::new());
                    key_error.set(None);
                }
                Err(e) => key_error.set(Some(super::server_error_message(&e))),
            }
            busy.set(false);
        });
    };

    let mut pending_delete: Signal<Option<i64>> = use_signal(|| None);
    let confirm_delete = move |id: i64| {
        spawn(async move {
            match delete_ssh_key(id).await {
                Ok(()) => {
                    key_error.set(None);
                    keys.write().retain(|k| k.id != id);
                    pending_delete.set(None);
                }
                Err(e) => key_error.set(Some(super::server_error_message(&e))),
            }
        });
    };

    let mut copied: Signal<Option<i64>> = use_signal(|| None);
    let copy_key = move |key: SshKeySummary| {
        spawn(async move {
            let script = format!(
                "await navigator.clipboard.writeText({}); return true;",
                serde_json::to_string(&key.public_key).unwrap_or_default()
            );
            if document::eval(&script).await.is_ok() {
                copied.set(Some(key.id));
            }
        });
    };

    rsx! {
        section { class: "git-section",
            h2 { "SSH key" }
            p { class: "muted",
                "One key for now. Add its public half to your git host: on GitHub, under your account's SSH keys, so it reaches all your repos (a deploy key only reaches one repo). To use a different key, delete this one first."
            }
            if keys().is_empty() {
                p { class: "muted", "No key yet." }
            } else {
                div { class: "sandbox-volume-list",
                    for key in keys() {
                        div { key: "{key.id}", class: "sandbox-volume-row git-key-row",
                            div { class: "sandbox-volume-summary",
                                span { class: "sandbox-volume-name", "{key.name}" }
                                span { class: "sandbox-volume-path", "{key.fingerprint}" }
                                code { class: "git-public-key", "{key.public_key}" }
                            }
                            div { class: "git-key-actions",
                                button {
                                    class: "git-key-copy",
                                    r#type: "button",
                                    onclick: {
                                        let key = key.clone();
                                        move |_| copy_key(key.clone())
                                    },
                                    if copied() == Some(key.id) { "Copied" } else { "Copy public key" }
                                }
                                super::TwoStepButton {
                                    armed: pending_delete() == Some(key.id),
                                    class: "sandbox-volume-delete",
                                    idle: "Delete",
                                    confirm: "Confirm delete?",
                                    on_arm: move |_| pending_delete.set(Some(key.id)),
                                    on_confirm: move |_| confirm_delete(key.id),
                                }
                            }
                        }
                    }
                }
            }

            if keys().is_empty() {
            form { class: "sandbox-volume-add-form git-key-form", onsubmit: add_key,
                h3 { if importing() { "Import a key" } else { "Add a key" } }
                label { r#for: "git-key-name", "Name" }
                input {
                    id: "git-key-name",
                    r#type: "text",
                    required: true,
                    placeholder: "github",
                    value: "{new_key_name}",
                    oninput: move |e| new_key_name.set(e.value()),
                }
                p { class: "muted", "Letters, digits, - or _. The key is at /etc/smelt/keys/<name> in the sandbox." }
                if importing() {
                    label { r#for: "git-key-private", "Private key" }
                    textarea {
                        id: "git-key-private",
                        class: "git-private-key-input",
                        required: true,
                        rows: 8,
                        placeholder: "-----BEGIN OPENSSH PRIVATE KEY-----",
                        value: "{private_key}",
                        oninput: move |e| private_key.set(e.value()),
                    }
                }
                super::ErrorText { message: key_error() }
                div { class: "git-key-form-buttons",
                    button {
                        r#type: "submit",
                        disabled: busy(),
                        if importing() { "Import key" } else { "Generate key" }
                    }
                    button {
                        r#type: "button",
                        class: "git-key-mode",
                        onclick: move |_| { importing.toggle(); key_error.set(None); },
                        if importing() { "Generate a new key instead" } else { "Import an existing key instead" }
                    }
                }
            }
            }
        }
    }
}

/// SME-283: the trust list shows a recorded key escaped — a key recorded
/// before the fix can be another key's invisible twin — with the hover
/// notice in the shared wording, and the `file/` rewrite keeps working
/// through the escape. The first tests in this file, inline per the plan.
#[cfg(all(test, feature = "server"))]
mod shown_tests {
    use super::*;

    fn decision(remote: &str, trusted: bool) -> RepoTrustSummary {
        RepoTrustSummary { remote: remote.to_string(), trusted }
    }

    #[test]
    fn test_the_trust_list_shows_a_recorded_key_escaped_with_its_notice() {
        let dirty = decision("git\u{200b}hub.com/o/r", true);
        let shown = remote_shown(&dirty);
        assert!(shown.contains("\\u{200b}"), "{shown}");
        assert!(!shown.contains('\u{200b}'), "raw U+200B in {shown:?}");
        assert_eq!(trust_row_notice(&dirty).as_deref(), Some(hidden_text_note(1).as_str()));

        let plain = decision("github.com/o/r", true);
        assert_eq!(remote_shown(&plain), "github.com/o/r");
        assert_eq!(trust_row_notice(&plain), None);

        let local = decision("file/tmp/x\u{202e}", false);
        assert_eq!(remote_shown(&local), "file:///tmp/x\\u{202e}");
        assert_eq!(trust_row_notice(&local).as_deref(), Some(hidden_text_note(1).as_str()));
    }
}
