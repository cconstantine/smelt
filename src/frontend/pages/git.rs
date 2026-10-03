use dioxus::prelude::*;

use crate::api::git::{
    delete_ssh_key, forget_repo_trust, generate_ssh_key, get_git_settings, import_ssh_key,
    save_git_identity,
};
use crate::frontend::Route;
use crate::git::{GitIdentity, RepoTrustSummary, SshKeySummary};

/// Git settings (SME-32): the commit identity and the SSH keys every
/// sandbox pod gets. A change reaches running pods straight away.
#[component]
pub fn GitSettingsPage() -> Element {
    let initial = use_resource(get_git_settings);
    let mut loaded = use_signal(|| false);
    let mut load_error: Signal<Option<String>> = use_signal(|| None);
    let mut keys: Signal<Vec<SshKeySummary>> = use_signal(Vec::new);
    let mut trust: Signal<Vec<RepoTrustSummary>> = use_signal(Vec::new);
    let mut trust_error: Signal<Option<String>> = use_signal(|| None);
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
    let mut request_delete = move |id: i64| {
        if pending_delete() == Some(id) {
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
        } else {
            pending_delete.set(Some(id));
        }
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
        div { class: "sandbox-volumes-page git-page",
            div { class: "sandbox-volumes-header",
                Link { to: Route::Home {}, class: "sandbox-volumes-back-link", "\u{2190} Back to conversations" }
                h1 { "Git" }
                p { class: "muted",
                    "Every sandbox gets these keys and this name and email, so the model can clone and push. Changes reach running sandboxes straight away."
                }
            }
            if let Some(err) = load_error() {
                p { class: "error", "{err}" }
            }
            if !loaded() {
                p { class: "muted", "Loading..." }
            } else {
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
                            Some(Err(err)) => rsx! { p { class: "error", "{err}" } },
                            None => rsx! {},
                        }
                        button { r#type: "submit", "Save" }
                    }
                }

                section { class: "git-section",
                    h2 { "Repos you've decided about" }
                    p { class: "muted",
                        "The model loads a repo's AGENTS.md into its instructions when it asks to; you're asked the first time for each repo. Forget a decision to be asked again."
                    }
                    if let Some(err) = trust_error() {
                        p { class: "error", "{err}" }
                    }
                    if trust().is_empty() {
                        p { class: "muted", "None yet. You're asked the first time the model wants to load a repo's AGENTS.md; repos you open with Work on a repo are trusted." }
                    } else {
                        div { class: "sandbox-volume-list",
                            for decision in trust() {
                                div { key: "{decision.remote}", class: "sandbox-volume-row",
                                    div { class: "sandbox-volume-summary",
                                        span { class: "sandbox-volume-name", {crate::git::remote_label(&decision.remote)} }
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
                                        button {
                                            class: if pending_delete() == Some(key.id) { "sandbox-volume-delete confirm" } else { "sandbox-volume-delete" },
                                            r#type: "button",
                                            onclick: move |_| request_delete(key.id),
                                            super::TwoStepLabel { armed: pending_delete() == Some(key.id), idle: "Delete", confirm: "Confirm delete?" }
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
                        if let Some(err) = key_error() {
                            p { class: "error", "{err}" }
                        }
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
    }
}
