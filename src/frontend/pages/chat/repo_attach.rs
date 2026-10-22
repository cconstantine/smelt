//! "Work on a repo": cloning a repo into a new conversation's sandbox.

use super::*;

/// Each conversation's "Work on a repo" in this tab: whether a clone is in
/// flight, and the last one's error (SME-263). Kept by conversation,
/// outside `ConversationState`, so a switch doesn't drop an answer that
/// arrives while another conversation is open, as `ChatErrors` keeps the
/// turn errors. Lost with `ChatPanel` (a reload, or leaving the page).
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct RepoAttaches {
    attaching: std::collections::HashSet<i64>,
    errors: HashMap<i64, String>,
}

impl RepoAttaches {
    /// A Clone in `id`: false when one is already in flight there;
    /// otherwise marks it in flight and clears `id`'s kept error.
    pub(super) fn start(&mut self, id: i64) -> bool {
        if !self.attaching.insert(id) {
            return false;
        }
        self.errors.remove(&id);
        true
    }

    /// `id`'s answer: no longer in flight, and its error kept (`Err`) or
    /// cleared (`Ok`).
    pub(super) fn finish(&mut self, id: i64, result: Result<(), String>) {
        self.attaching.remove(&id);
        match result {
            Ok(()) => {
                self.errors.remove(&id);
            }
            Err(e) => {
                self.errors.insert(id, e);
            }
        }
    }

    pub(super) fn attaching(&self, id: i64) -> bool {
        self.attaching.contains(&id)
    }

    pub(super) fn error(&self, id: i64) -> Option<&str> {
        self.errors.get(&id).map(String::as_str)
    }

    /// A send in `id`: its kept error goes (the user has moved on from the
    /// form); a clone in flight stays.
    pub(super) fn clear_error(&mut self, id: i64) {
        self.errors.remove(&id);
    }
}

/// The "Work on a repo" form a new conversation offers: clones the repo
/// (starting the sandbox if need be) while the user writes their first
/// message (SME-32, SME-49). Cloning is the panel's (`on_attach`).
#[component]
pub(super) fn RepoAttach(
    selected: Memo<Option<i64>>,
    state: Store<ConversationState>,
    attaches: Signal<RepoAttaches>,
    on_attach: EventHandler<()>,
) -> Element {
    let mut repo_url = state.repo_url();
    let mut repo_branch = state.repo_branch();
    let mut repo_dir = state.repo_dir();
    let repo_attaching = move || selected().is_some_and(|id| attaches.read().attaching(id));
    let repo_attach_error = move || selected().and_then(|id| attaches.read().error(id).map(str::to_string));
    rsx! {
        form {
            class: "repo-attach",
            onsubmit: move |event| {
                event.prevent_default();
                on_attach.call(());
            },
            label { r#for: "repo-attach-url", "Work on a repo" }
            div { class: "repo-attach-fields",
                input {
                    id: "repo-attach-url",
                    r#type: "text",
                    required: true,
                    placeholder: "git@github.com:owner/repo.git",
                    value: "{repo_url}",
                    oninput: move |e| repo_url.set(e.value()),
                }
                input {
                    class: "repo-attach-branch",
                    r#type: "text",
                    placeholder: "branch (optional)",
                    aria_label: "Branch",
                    value: "{repo_branch}",
                    oninput: move |e| repo_branch.set(e.value()),
                }
                input {
                    class: "repo-attach-branch",
                    r#type: "text",
                    placeholder: "directory (optional)",
                    aria_label: "Directory under /workspace",
                    value: "{repo_dir}",
                    oninput: move |e| repo_dir.set(e.value()),
                }
                button {
                    r#type: "submit",
                    disabled: repo_attaching(),
                    if repo_attaching() { "Cloning\u{2026}" } else { "Clone" }
                }
            }
            if repo_attaching() {
                p { class: "muted", "Starting the sandbox and cloning. You can write your first message meanwhile." }
            }
            if let Some(err) = repo_attach_error() {
                pre { class: "error repo-attach-error", "{err}" }
            }
        }
    }
}
