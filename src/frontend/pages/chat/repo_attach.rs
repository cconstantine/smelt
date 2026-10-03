//! "Work on a repo": cloning a repo into a new conversation's sandbox.

use super::*;

/// The "Work on a repo" form a new conversation offers: clones the repo
/// (starting the sandbox if need be) while the user writes their first
/// message (SME-32, SME-49).
#[component]
pub(super) fn RepoAttach(
    selected: Memo<Option<i64>>,
    mut repo_url: Signal<String>,
    mut repo_branch: Signal<String>,
    mut repo_dir: Signal<String>,
    mut repo_attaching: Signal<bool>,
    mut repo_attach_error: Signal<Option<String>>,
) -> Element {
    rsx! {
        form {
            class: "repo-attach",
            onsubmit: move |event| {
                event.prevent_default();
                let Some(id) = selected() else { return };
                if repo_attaching() {
                    return;
                }
                let url = repo_url();
                let branch = repo_branch();
                let dir = repo_dir();
                repo_attaching.set(true);
                repo_attach_error.set(None);
                spawn(async move {
                    let result = attach_repo(id, url, branch, dir).await;
                    // The user may have moved on to another conversation.
                    if selected() != Some(id) {
                        return;
                    }
                    match result {
                        Ok(_) => {
                            repo_url.set(String::new());
                            repo_branch.set(String::new());
                            repo_dir.set(String::new());
                        }
                        Err(e) => repo_attach_error.set(Some(server_error_message(&e))),
                    }
                    repo_attaching.set(false);
                });
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
