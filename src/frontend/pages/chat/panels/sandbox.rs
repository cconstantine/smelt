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

/// What a terminal's titlebar says about its most recent command
/// (SME-144): the titlebar sits outside the scrolling body, so this stays
/// in view however much the command prints.
#[derive(Clone, Debug, PartialEq)]
pub(in super::super) enum CommandIndicator {
    /// No command yet: no pill.
    None,
    Running,
    Exited(i32),
    /// The sandbox lost track of it: no exit status.
    Lost,
    /// A status the panel doesn't know, or a finish with no code.
    Other(String),
}

/// The indicator for `terminal`'s last command.
pub(in super::super) fn latest_command_indicator(terminal: &SandboxTerminalPanelEntry) -> CommandIndicator {
    let Some(last) = terminal.commands.last() else {
        return CommandIndicator::None;
    };
    match (last.status.as_str(), last.exit_code) {
        ("running", _) => CommandIndicator::Running,
        ("finished", Some(code)) => CommandIndicator::Exited(code),
        ("lost", _) => CommandIndicator::Lost,
        (other, _) => CommandIndicator::Other(other.to_string()),
    }
}

impl CommandIndicator {
    /// The pill's text: the word carries the meaning, never the colour
    /// alone.
    pub(in super::super) fn label(&self) -> String {
        match self {
            Self::None => String::new(),
            Self::Running => "running".to_string(),
            Self::Exited(code) => format!("exit {code}"),
            Self::Lost => "lost".to_string(),
            Self::Other(status) => status.clone(),
        }
    }

    /// A glyph shown before the label, hidden from screen readers.
    pub(in super::super) fn glyph(&self) -> &'static str {
        match self {
            Self::None => "",
            Self::Running => "\u{25cf}",
            Self::Exited(0) => "\u{2713}",
            Self::Exited(_) => "\u{2717}",
            Self::Lost => "\u{26a0}",
            Self::Other(_) => "\u{2022}",
        }
    }

    /// The pill's classes.
    pub(in super::super) fn class(&self) -> &'static str {
        match self {
            Self::None => "task-terminal-command",
            Self::Running => "task-terminal-command task-terminal-command-running",
            Self::Exited(0) => "task-terminal-command task-terminal-command-ok",
            Self::Exited(_) => "task-terminal-command task-terminal-command-failed",
            Self::Lost => "task-terminal-command task-terminal-command-lost",
            Self::Other(_) => "task-terminal-command task-terminal-command-other",
        }
    }

    /// The pill's tooltip and accessible name.
    pub(in super::super) fn title(&self, command: &str) -> String {
        let state = match self {
            Self::None => return String::new(),
            Self::Running => "running".to_string(),
            Self::Exited(code) => format!("exited with status {code}"),
            Self::Lost => {
                "lost: it couldn't be sent to the sandbox, so it never ran; no exit status".to_string()
            }
            Self::Other(status) => status.clone(),
        };
        format!("Last command: {} \u{b7} {state}", short_command(command))
    }
}

/// How much of a command the pill's tooltip shows: its first line with
/// anything in it, cut at 120 characters, with "\u{2026}" when anything was
/// left out. A model-written heredoc can be many KB, and the tooltip is also
/// the accessible name. Wrapped in a first-strong isolate, so a bidi control
/// in the command (or one the cut left open) can't reorder the status after
/// it; "(empty)" for a command with nothing in it (SME-144 review 2).
fn short_command(command: &str) -> String {
    const MAX: usize = 120;
    let trimmed = command.trim();
    let Some(first) = trimmed.lines().map(str::trim_end).find(|line| !line.trim().is_empty()) else {
        return "(empty)".to_string();
    };
    let first = first.trim_start();
    let mut short: String = first.chars().take(MAX).collect();
    if first.chars().count() > MAX || trimmed.len() > first.len() {
        short.push('\u{2026}');
    }
    format!("\u{2068}{short}\u{2069}")
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
                        super::TwoStepButton {
                            armed: pending_pod_stop() == Some(pod.pod_id),
                            class: "pod-stop",
                            idle: "Stop sandbox",
                            confirm: "Confirm stop?",
                            title: "Stop this pod. Its terminals and any files outside /workspace and mounted volumes are lost.",
                            on_arm: move |_| on_stop_pod.call(pod.pod_id),
                            on_confirm: move |_| on_stop_pod.call(pod.pod_id),
                        }
                    }
                    super::ErrorText { message: pod_stop_error() }
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
                                    {
                                        let indicator = latest_command_indicator(&terminal);
                                        let command = terminal.commands.last().map(|c| c.command.clone()).unwrap_or_default();
                                        let title = indicator.title(&command);
                                        rsx! {
                                            if indicator != CommandIndicator::None {
                                                span {
                                                    class: "{indicator.class()}",
                                                    title: "{title}",
                                                    aria_label: "{title}",
                                                    role: "status",
                                                    span { class: "task-terminal-command-glyph", aria_hidden: "true", "{indicator.glyph()}" }
                                                    "{indicator.label()}"
                                                }
                                            }
                                        }
                                    }
                                    span { class: "task-terminal-status", "{terminal.status}" }
                                }
                                TerminalBody { terminal }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One terminal's output, following its bottom as it grows unless the user
/// scrolled up to read (see `StickyBottom`). Keyed by the terminal, so its
/// scroll state goes with it.
#[component]
fn TerminalBody(terminal: SandboxTerminalPanelEntry) -> Element {
    let scroll = use_sticky_bottom();
    // What makes the body taller: a new command, or a new line.
    let size: usize = terminal.commands.iter().map(|c| 1 + c.output.len()).sum();
    use_effect(use_reactive!(|size| {
        let _ = size;
        if scroll.is_stuck()
            && let Some(el) = scroll.el()
        {
            spawn(scroll_to_bottom(el));
        }
    }));
    rsx! {
        div {
            class: "task-terminal-body",
            onmounted: move |evt| scroll.mounted(evt),
            onscroll: move |evt: Event<ScrollData>| scroll.scrolled(&evt.data()),
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
