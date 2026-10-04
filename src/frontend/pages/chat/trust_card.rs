//! The trust card a repo's AGENTS.md waits on (SME-32).

use super::*;

/// One card per AGENTS.md waiting for the user's trust, and the last
/// Trust / Don't trust failure.
#[component]
pub(super) fn TrustCards(
    selected: Memo<Option<i64>>,
    repos: Signal<Vec<RepoSummary>>,
    mut repo_action_error: Signal<Option<String>>,
) -> Element {
    rsx! {
        // A repo's AGENTS.md waits for the user's trust before it
        // becomes instructions the model follows (SME-32).
        for (repo, request) in repos().into_iter().flat_map(|r| r.trust_requests.clone().into_iter().map(move |q| (r.clone(), q))) {
            div { key: "trust-{request.id}", class: "trust-card", role: "group", aria_label: "Trust {repo.url}?",
                p { class: "trust-card-question",
                    "Trust "
                    code { "{repo.url}" }
                    "?"
                }
                p { class: "muted",
                    "The model wants to load this AGENTS.md as instructions it follows on every turn. Trust loads exactly the file below, and later ones from this repo load without asking. Only trust repos whose instructions you're happy for the model to follow."
                }
                details { class: "trust-card-preview", open: true,
                    summary { "{request.path}" }
                    pre { "{request.content}" }
                }
                div { class: "trust-card-buttons",
                    button {
                        class: "trust-card-trust",
                        r#type: "button",
                        onclick: {
                            let request_id = request.id;
                            let shown_hash = request.hash.clone();
                            move |_| {
                                let Some(id) = selected() else { return };
                                let shown_hash = shown_hash.clone();
                                repo_action_error.set(None);
                                spawn(async move {
                                    if let Err(e) = decide_repo_trust(id, request_id, shown_hash, true).await
                                        // Not if the user has moved on to another conversation.
                                        && selected() == Some(id)
                                    {
                                        repo_action_error.set(Some(server_error_message(&e)));
                                    }
                                });
                            }
                        },
                        "Trust"
                    }
                    button {
                        class: "trust-card-decline",
                        r#type: "button",
                        onclick: {
                            let request_id = request.id;
                            let shown_hash = request.hash.clone();
                            move |_| {
                                let Some(id) = selected() else { return };
                                let shown_hash = shown_hash.clone();
                                repo_action_error.set(None);
                                spawn(async move {
                                    if let Err(e) = decide_repo_trust(id, request_id, shown_hash, false).await
                                        // Not if the user has moved on to another conversation.
                                        && selected() == Some(id)
                                    {
                                        repo_action_error.set(Some(server_error_message(&e)));
                                    }
                                });
                            }
                        },
                        "Don't trust"
                    }
                }
            }
        }
        if let Some(err) = repo_action_error() {
            p { class: "error", "{err}" }
        }
    }
}
