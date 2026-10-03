//! "Work on a repo": cloning a repo into a new conversation's sandbox.

use super::*;

/// The "Work on a repo" form a new conversation offers: clones the repo
/// (starting the sandbox if need be) while the user writes their first
/// message (SME-32, SME-49). Cloning is the panel's (`on_attach`).
#[component]
pub(super) fn RepoAttach(
    mut repo_url: Signal<String>,
    mut repo_branch: Signal<String>,
    mut repo_dir: Signal<String>,
    repo_attaching: Signal<bool>,
    repo_attach_error: Signal<Option<String>>,
    on_attach: EventHandler<()>,
) -> Element {
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
