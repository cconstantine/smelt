//! The sandbox panel: pods, terminals, repos.

use super::super::*;

/// What the sandbox panel says about a repo's `AGENTS.md` files, if it
/// has any: which the model has loaded.
pub(in super::super) fn instructions_label(repo: &RepoSummary) -> Option<String> {
    if !repo.loaded_instructions.is_empty() {
        Some(format!("Loaded {}", repo.loaded_instructions.join(", ")))
    } else if !repo.agents_files.is_empty() {
        Some("AGENTS.md not loaded".to_string())
    } else {
        None
    }
}

pub(in super::super) fn repo_status_class(status: RepoStatus) -> &'static str {
    match status {
        RepoStatus::Cloning => "cloning",
        RepoStatus::Ready => "ready",
        RepoStatus::Failed => "failed",
    }
}

/// A repo's state under its path in the sandbox panel.
pub(in super::super) fn repo_detail(repo: &RepoSummary) -> String {
    match repo.status {
        RepoStatus::Cloning => match &repo.requested_branch {
            Some(branch) => format!("Cloning {branch}\u{2026}"),
            None => "Cloning\u{2026}".to_string(),
        },
        RepoStatus::Failed => "Clone failed".to_string(),
        RepoStatus::Ready => {
            let branch = repo.branch.clone().unwrap_or_default();
            let commit: String = repo.commit.clone().unwrap_or_default().chars().take(7).collect();
            format!("{branch} \u{b7} {commit}")
        }
    }
}

/// The sandbox panel: the conversation's repos, its pod with a Stop
/// button and preview links, and each terminal's commands and output.
/// Stopping a pod is the panel's (`on_stop_pod`).
#[component]
pub(in super::super) fn SandboxPanel(
    selected: Memo<Option<i64>>,
    state: Store<ConversationState>,
    on_stop_pod: EventHandler<i64>,
) -> Element {
    let repos = state.repos();
    let sandbox_pods = state.sandbox_pods();
    let sandbox_terminals = state.sandbox_terminals();
    let pending_pod_stop = state.pending_pod_stop();
    let pod_stop_error = state.pod_stop_error();
    let mut terminal_body_els = state.terminal_body_els();
    let mut terminal_body_stuck = state.terminal_body_stuck();
    rsx! {
        aside { class: "sandbox-panel",
            h3 { "Sandbox" }
            // The conversation's repos, pod or not: /workspace and
            // its checkouts outlive the pod, and a clone that
            // failed to start a sandbox must still say so.
                    if !repos().is_empty() {
                        div { class: "sandbox-repos",
                            for repo in repos() {
                                div {
                                    key: "{repo.id}",
                                    class: "sandbox-repo sandbox-repo-{repo_status_class(repo.status)}",
                                    title: "{repo.url}",
                                    code { class: "sandbox-repo-path", "{repo.path}" }
                                    span { class: "sandbox-repo-detail", "{repo_detail(&repo)}" }
                                    if let Some(label) = instructions_label(&repo) {
                                        span {
                                            class: "sandbox-repo-instructions",
                                            title: "The model loads a repo's AGENTS.md files with load_instructions. Loaded ones are in its context on every turn; see the context view.",
                                            "{label}"
                                        }
                                    }
                                    if let Some(err) = repo.error.clone() {
                                        pre { class: "sandbox-repo-error", "{err}" }
                                    }
                                }
                            }
                        }
                    }
            // A conversation has at most one live pod (see
            // SME-11's "One pod
            // per conversation") — straight through, no tab
            // bar needed to pick between pods anymore.
            for pod in sandbox_pods() {
                div { key: "{pod.pod_id}", class: "sandbox-pod",
                    div { class: "sandbox-pod-header",
                        span { class: "sandbox-pod-status", "{pod.status}" }
                        button {
                            class: if pending_pod_stop() == Some(pod.pod_id) { "pod-stop confirm" } else { "pod-stop" },
                            r#type: "button",
                            title: "Stop this pod. Its terminals and any files outside /workspace and mounted volumes are lost.",
                            onclick: move |_| on_stop_pod.call(pod.pod_id),
                            super::TwoStepLabel { armed: pending_pod_stop() == Some(pod.pod_id), idle: "Stop sandbox", confirm: "Confirm stop?" }
                        }
                    }
                    if let Some(err) = pod_stop_error() {
                        p { class: "error", "{err}" }
                    }
                    if !pod.previews.is_empty() {
                        div { class: "sandbox-previews",
                            for preview in pod.previews.clone() {
                                a {
                                    key: "{preview.url}",
                                    class: "sandbox-preview",
                                    href: "{preview.url}",
                                    target: "_blank",
                                    rel: "noopener noreferrer",
                                    title: "Open what's running on {preview.label()} in the sandbox, in a new tab",
                                    "Open preview · {preview.label()}"
                                }
                            }
                        }
                    }
                    div { class: "task-terminal-stack",
                        for terminal in sandbox_terminals().into_iter().filter(|t| t.pod_id == pod.pod_id) {
                            div {
                                key: "{terminal.terminal_id}",
                                class: "task-terminal",
                                div { class: "task-terminal-titlebar",
                                    span { class: "task-terminal-dots",
                                        span { class: "dot dot-red" }
                                        span { class: "dot dot-yellow" }
                                        span { class: "dot dot-green" }
                                    }
                                    span { class: "task-terminal-id", "terminal {terminal.terminal_id}" }
                                    span { class: "task-terminal-status", "{terminal.status}" }
                                }
                                div {
                                    class: "task-terminal-body",
                                    onmounted: {
                                        let terminal_id = terminal.terminal_id;
                                        move |evt| {
                                            terminal_body_els.write().insert(terminal_id, evt);
                                        }
                                    },
                                    onscroll: {
                                        let terminal_id = terminal.terminal_id;
                                        move |evt: Event<ScrollData>| {
                                            let d = evt.data();
                                            terminal_body_stuck
                                                .write()
                                                .insert(
                                                    terminal_id,
                                                    is_scrolled_to_bottom(
                                                        d.scroll_top(),
                                                        d.scroll_height() as f64,
                                                        d.client_height() as f64,
                                                    ),
                                                );
                                        }
                                    },
                                    if terminal.commands.is_empty() {
                                        span { class: "task-terminal-empty", "no commands yet" }
                                    }
                                    for (ci , command) in terminal.commands.iter().enumerate() {
                                        div { key: "{command.command_id}", class: "sandbox-command-block",
                                            div { class: "sandbox-command-header",
                                                code { "{command.command}" }
                                                span { class: "sandbox-command-status",
                                                    if let Some(code) = command.exit_code {
                                                        "{command.status} ({code})"
                                                    } else {
                                                        "{command.status}"
                                                    }
                                                }
                                            }
                                            for (i , line) in command.output.iter().enumerate() {
                                                div {
                                                    key: "line-{i}",
                                                    class: if line.stream == "stderr" { "task-terminal-line task-terminal-line-stderr" } else { "task-terminal-line" },
                                                    "{line.data}"
                                                }
                                            }
                                            if ci == terminal.commands.len() - 1 && command.status == "running" {
                                                span { class: "task-terminal-cursor" }
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
    }
}
