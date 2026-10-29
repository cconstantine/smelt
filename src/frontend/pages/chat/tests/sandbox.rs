use super::*;

#[test]
fn test_storage_line_names_both_claims_their_size_or_removal() {
    use crate::api::pods::{StorageKind, StoredVolume};
    let volume = |kind, size: Option<&str>, deleting| StoredVolume {
        kind,
        size: size.map(str::to_string),
        deleting,
    };
    assert_eq!(
        storage_line(&[
            volume(StorageKind::Workspace, Some("20Gi"), false),
            volume(StorageKind::Docker, Some("20Gi"), false),
        ]),
        "Sandbox storage: /workspace 20Gi \u{b7} Docker images 20Gi",
        "the panel's line, however the two listed"
    );
    assert_eq!(
        storage_line(&[volume(StorageKind::Docker, Some("20Gi"), true)]),
        "Sandbox storage: Docker images removing\u{2026}",
        "a claim being deleted reads removing, not its size"
    );
    assert_eq!(
        storage_line(&[volume(StorageKind::Workspace, None, false)]),
        "Sandbox storage: /workspace unknown size",
        "a claim with no size on the object says so rather than going blank"
    );
}

fn test_output_line(stream: &str, data: &str) -> SandboxOutputLine {
    SandboxOutputLine {
        stream: stream.to_string(),
        data: data.to_string(),
        seq: 0,
    }
}

fn test_output_line_entry(stream: &str, data: &str) -> SandboxOutputLinePanelEntry {
    SandboxOutputLinePanelEntry {
        stream: stream.to_string(),
        data: data.to_string(),
        seq: None,
    }
}

#[test]
fn test_merge_sandbox_snapshot_carries_each_pods_previews() {
    let mut pods = vec![SandboxPodPanelEntry {
        pod_id: 1,
        status: "Running".to_string(),
        previews: vec![preview(8080)],
    }];
    let mut terminals = Vec::new();
    let snapshot = SandboxSnapshot {
        pods: vec![
            SandboxPodSummary {
                pod_id: 1,
                status: "Running".to_string(),
                terminals: Vec::new(),
                previews: vec![preview(3000)],
            },
            SandboxPodSummary {
                pod_id: 2,
                status: "Running".to_string(),
                terminals: Vec::new(),
                previews: vec![preview(5173)],
            },
        ],
    };
    merge_sandbox_snapshot(&mut pods, &mut terminals, snapshot);
    assert_eq!(pods[0].previews, vec![preview(3000)], "the snapshot's list replaces the old one");
    assert_eq!(pods[1].previews, vec![preview(5173)]);
}

#[test]
fn test_apply_sandbox_preview_update_replaces_only_its_pods_list() {
    let mut pods = vec![
        SandboxPodPanelEntry { pod_id: 1, status: "Running".to_string(), previews: Vec::new() },
        SandboxPodPanelEntry { pod_id: 2, status: "Running".to_string(), previews: vec![preview(9000)] },
    ];
    apply_sandbox_preview_update(&mut pods, 1, vec![preview(3000), preview(5173)]);
    assert_eq!(pods[0].previews, vec![preview(3000), preview(5173)]);
    assert_eq!(pods[1].previews, vec![preview(9000)], "another pod's previews are untouched");

    apply_sandbox_preview_update(&mut pods, 99, vec![preview(1)]);
    assert_eq!(pods.len(), 2, "an unknown pod isn't invented");
}

#[test]
fn test_a_pod_status_update_keeps_its_previews() {
    let mut pods = vec![SandboxPodPanelEntry {
        pod_id: 1,
        status: "Pending".to_string(),
        previews: vec![preview(3000)],
    }];
    let mut terminals = Vec::new();
    apply_sandbox_pod_update(&mut pods, &mut terminals, 1, "Running".to_string(), false);
    assert_eq!(pods[0].previews, vec![preview(3000)]);
}

#[test]
fn test_merge_sandbox_snapshot_flattens_pods_and_terminals_and_hydrates_command_history() {
    let mut pods = Vec::new();
    let mut terminals = Vec::new();
    let snapshot = SandboxSnapshot {
        pods: vec![SandboxPodSummary {
            pod_id: 1,
            status: "Running".to_string(),
            terminals: vec![SandboxTerminalSummary {
                terminal_id: 2,
                pod_id: 1,
                status: "connected".to_string(),
                commands: vec![
                    SandboxCommandSummary {
                        command_id: "cmd-1".to_string(),
                        command: "cd /tmp".to_string(),
                        status: "finished".to_string(),
                        exit_code: Some(0),
                        output: Vec::new(),
                    },
                    SandboxCommandSummary {
                        command_id: "cmd-2".to_string(),
                        command: "echo hi".to_string(),
                        status: "finished".to_string(),
                        exit_code: Some(0),
                        // Deliberately interleaved (stdout, stderr, stdout) —
                        // proves the snapshot's own order survives the merge,
                        // rather than getting bucketed into "all stdout, then
                        // all stderr".
                        output: vec![
                            test_output_line("stdout", "hi"),
                            test_output_line("stderr", "uh oh"),
                            test_output_line("stdout", "bye"),
                        ],
                    },
                ],
            }],
            previews: Vec::new(),
        }],
    };

    merge_sandbox_snapshot(&mut pods, &mut terminals, snapshot);

    assert_eq!(pods.len(), 1);
    assert_eq!(pods[0].pod_id, 1);
    assert_eq!(pods[0].status, "Running");
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].terminal_id, 2);
    assert_eq!(
        terminals[0]
            .commands
            .iter()
            .map(|c| c.command.as_str())
            .collect::<Vec<_>>(),
        vec!["cd /tmp", "echo hi"],
        "history should preserve the snapshot's own (oldest-first) order"
    );
    assert_eq!(
        terminals[0].commands[1]
            .output
            .iter()
            .map(|l| (l.stream.as_str(), l.data.as_str()))
            .collect::<Vec<_>>(),
        vec![("stdout", "hi"), ("stderr", "uh oh"), ("stdout", "bye")],
        "output order must match the snapshot's, not get split by stream"
    );
}

#[test]
fn test_merge_sandbox_snapshot_is_authoritative_over_existing_entries() {
    let mut pods = vec![SandboxPodPanelEntry {
        pod_id: 1,
        status: "Pending".to_string(),
        previews: Vec::new(),
    }];
    let mut terminals = Vec::new();
    let snapshot = SandboxSnapshot {
        pods: vec![SandboxPodSummary {
            pod_id: 1,
            status: "Running".to_string(),
            terminals: Vec::new(),
            previews: Vec::new(),
        }],
    };

    merge_sandbox_snapshot(&mut pods, &mut terminals, snapshot);

    assert_eq!(
        pods.len(),
        1,
        "an existing pod should be updated, not duplicated"
    );
    assert_eq!(pods[0].status, "Running");
}

/// A pod or terminal that went away while the tab was disconnected
/// never gets its live `terminated` event; the reconnect's snapshot
/// leaving it out is the only sign (SME-43).
#[test]
fn test_merge_sandbox_snapshot_drops_pods_and_terminals_it_no_longer_lists() {
    let pod = |pod_id| SandboxPodPanelEntry { pod_id, status: "Running".to_string(), previews: Vec::new() };
    let terminal = |terminal_id, pod_id| SandboxTerminalPanelEntry {
        terminal_id,
        pod_id,
        status: "connected".to_string(),
        commands: Vec::new(),
    };
    let mut pods = vec![pod(1), pod(2)];
    let mut terminals = vec![terminal(10, 1), terminal(11, 1), terminal(20, 2)];
    let snapshot = SandboxSnapshot {
        pods: vec![SandboxPodSummary {
            pod_id: 1,
            status: "Running".to_string(),
            terminals: vec![SandboxTerminalSummary {
                terminal_id: 10,
                pod_id: 1,
                status: "connected".to_string(),
                commands: Vec::new(),
            }],
            previews: Vec::new(),
        }],
    };

    merge_sandbox_snapshot(&mut pods, &mut terminals, snapshot);

    assert_eq!(pods.iter().map(|p| p.pod_id).collect::<Vec<_>>(), vec![1]);
    assert_eq!(terminals.iter().map(|t| t.terminal_id).collect::<Vec<_>>(), vec![10]);
}

#[test]
fn test_apply_sandbox_pod_update_upserts_when_not_terminated() {
    let mut pods = Vec::new();
    let mut terminals = Vec::new();
    apply_sandbox_pod_update(&mut pods, &mut terminals, 1, "Running".to_string(), false);
    assert_eq!(pods.len(), 1);
    assert_eq!(pods[0].status, "Running");

    apply_sandbox_pod_update(&mut pods, &mut terminals, 1, "Running".to_string(), false);
    assert_eq!(
        pods.len(),
        1,
        "a repeat update for the same pod_id should update, not duplicate"
    );
}

#[test]
fn test_apply_sandbox_pod_update_removes_pod_and_its_terminals_when_terminated() {
    let mut pods = vec![SandboxPodPanelEntry {
        pod_id: 1,
        status: "Running".to_string(),
        previews: Vec::new(),
    }];
    let mut terminals = vec![
        test_sandbox_terminal_entry(10, 1),
        test_sandbox_terminal_entry(20, 2),
    ];

    apply_sandbox_pod_update(&mut pods, &mut terminals, 1, "terminated".to_string(), true);

    assert!(
        pods.is_empty(),
        "the terminated pod should be removed, not just relabeled"
    );
    assert_eq!(
        terminals.iter().map(|t| t.terminal_id).collect::<Vec<_>>(),
        vec![20],
        "only terminal_id 10 (under the terminated pod) should be dropped; pod 2's terminal is untouched"
    );
}

#[test]
fn test_apply_sandbox_terminal_update_upserts_when_not_terminated() {
    let mut terminals = Vec::new();
    apply_sandbox_terminal_update(&mut terminals, 1, 10, "connected".to_string(), false);
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].status, "connected");
}

#[test]
fn test_apply_sandbox_terminal_update_removes_only_the_matching_terminal_when_terminated() {
    let mut terminals = vec![
        test_sandbox_terminal_entry(10, 1),
        test_sandbox_terminal_entry(20, 1),
    ];

    apply_sandbox_terminal_update(&mut terminals, 1, 10, "disconnected".to_string(), true);

    assert_eq!(
        terminals.iter().map(|t| t.terminal_id).collect::<Vec<_>>(),
        vec![20],
        "terminating one terminal should not affect its sibling in the same pod"
    );
}

#[test]
fn test_apply_sandbox_command_update_with_command_appends_a_new_history_entry() {
    let mut terminals = vec![test_sandbox_terminal_entry(10, 1)];
    terminals[0]
        .commands
        .push(test_sandbox_command_entry("cmd-old", "sleep 30"));
    terminals[0].commands[0].output = vec![test_output_line_entry(
        "stdout",
        "output from a previous command",
    )];

    apply_sandbox_command_update(
        &mut terminals,
        10,
        "cmd-new".to_string(),
        Some("echo hi".to_string()),
        "running".to_string(),
        None,
        None,
        None, None,
    );

    assert_eq!(
        terminals[0]
            .commands
            .iter()
            .map(|c| c.command_id.as_str())
            .collect::<Vec<_>>(),
        vec!["cmd-old", "cmd-new"],
        "a new command should be appended to the terminal's history, not replace it"
    );
    assert_eq!(
        terminals[0].commands[0].output,
        vec![test_output_line_entry(
            "stdout",
            "output from a previous command"
        )],
        "an earlier command's own output should be untouched by a later command starting"
    );
    assert!(terminals[0].commands[1].output.is_empty());
}

#[test]
fn test_apply_sandbox_command_update_without_command_appends_a_line_to_the_most_recent_command()
{
    let mut terminals = vec![test_sandbox_terminal_entry(10, 1)];
    terminals[0]
        .commands
        .push(test_sandbox_command_entry("cmd-1", "echo hi"));

    apply_sandbox_command_update(
        &mut terminals,
        10,
        "cmd-1".to_string(),
        None,
        "running".to_string(),
        None,
        Some("stdout".to_string()),
        Some("hi".to_string()), None,
    );

    assert_eq!(
        terminals[0].commands[0].output,
        vec![test_output_line_entry("stdout", "hi")]
    );
    assert_eq!(
        terminals[0].commands[0].command, "echo hi",
        "an output-line update shouldn't touch the already-known command text"
    );
}

#[test]
fn test_apply_sandbox_command_update_preserves_arrival_order_across_streams() {
    let mut terminals = vec![test_sandbox_terminal_entry(10, 1)];
    terminals[0]
        .commands
        .push(test_sandbox_command_entry("cmd-1", "sh -c '...'"));

    for (stream, data) in [("stdout", "one"), ("stderr", "uh oh"), ("stdout", "two")] {
        apply_sandbox_command_update(
            &mut terminals,
            10,
            "cmd-1".to_string(),
            None,
            "running".to_string(),
            None,
            Some(stream.to_string()),
            Some(data.to_string()), None,
        );
    }

    assert_eq!(
        terminals[0].commands[0].output,
        vec![
            test_output_line_entry("stdout", "one"),
            test_output_line_entry("stderr", "uh oh"),
            test_output_line_entry("stdout", "two"),
        ],
        "live updates must interleave in arrival order, not group by stream"
    );
}

#[test]
fn test_apply_sandbox_command_update_finish_sets_status_and_exit_code_on_the_most_recent_command()
 {
    let mut terminals = vec![test_sandbox_terminal_entry(10, 1)];
    terminals[0]
        .commands
        .push(test_sandbox_command_entry("cmd-1", "echo hi"));

    apply_sandbox_command_update(
        &mut terminals,
        10,
        "cmd-1".to_string(),
        None,
        "finished".to_string(),
        Some(0),
        None,
        None, None,
    );

    assert_eq!(terminals[0].commands[0].status, "finished");
    assert_eq!(terminals[0].commands[0].exit_code, Some(0));
}
