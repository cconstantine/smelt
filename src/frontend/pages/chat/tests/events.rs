use super::*;

/// Runs `f` with a fresh `ConversationState` store. A store, like a signal,
/// needs an owner, so it's created inside a `VirtualDom`'s root scope.
fn with_state(f: impl FnOnce(Store<ConversationState>)) {
    fn app() -> Element {
        rsx! {}
    }
    let mut dom = VirtualDom::new(app);
    dom.rebuild_in_place();
    dom.in_scope(ScopeId::APP, || f(Store::new(ConversationState::default())));
}

/// The pods and terminals are two fields of one store, which lends out one
/// write at a time: changing both at once has to go through
/// `change_sandbox_panel` (a reconnect's snapshot, a pod going away).
#[test]
fn test_change_sandbox_panel_changes_the_pods_and_terminals_together() {
    with_state(|state| {
        state.sandbox_terminals().set(vec![SandboxTerminalPanelEntry {
            terminal_id: 9,
            pod_id: 1,
            status: "open".to_string(),
            commands: Vec::new(),
        }]);
        let snapshot = SandboxSnapshot {
            pods: vec![SandboxPodSummary {
                pod_id: 2,
                status: "Running".to_string(),
                terminals: vec![SandboxTerminalSummary {
                    terminal_id: 20,
                    pod_id: 2,
                    status: "open".to_string(),
                    commands: Vec::new(),
                }],
                previews: Vec::new(),
            }],
        };
        change_sandbox_panel(state, |pods, terminals| merge_sandbox_snapshot(pods, terminals, snapshot));
        assert_eq!(state.sandbox_pods().peek().iter().map(|p| p.pod_id).collect::<Vec<_>>(), vec![2]);
        assert_eq!(
            state.sandbox_terminals().peek().iter().map(|t| (t.terminal_id, t.pod_id)).collect::<Vec<_>>(),
            vec![(20, 2)],
            "the snapshot's terminals replace the old ones, and stay in the store"
        );
    });
}

fn text_message(id: i64, role: &str, text: &str) -> Message {
    message_with_blocks(id, role, vec![ContentBlock::Text { text: text.to_string() }])
}

fn repo(id: i64) -> RepoSummary {
    RepoSummary {
        id,
        url: format!("https://example.com/r{id}.git"),
        path: format!("/workspace/r{id}"),
        requested_branch: None,
        branch: None,
        commit: None,
        status: RepoStatus::Ready,
        error: None,
        agents_files: vec![],
        loaded_instructions: vec![],
        trust_requests: vec![],
    }
}

#[test]
fn test_apply_event_messages_appended_adds_them_once_and_refreshes_the_sidebar() {
    with_state(|state| {
        state.messages().set(vec![text_message(1, "user", "hi")]);
        let effect = apply_event(
            state,
            ConversationEvent::MessagesAppended { messages: vec![text_message(1, "user", "hi"), text_message(2, "user", "more")] },
        );
        assert_eq!(effect, EventEffect::ConversationsChanged);
        let ids: Vec<i64> = state.messages().peek().iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![1, 2], "the row already shown isn't added twice");
    });
}

#[test]
fn test_apply_event_a_saved_reply_replaces_its_streaming_copy() {
    with_state(|state| {
        state.streaming_reply().set(Some("partial".to_string()));
        apply_event(state, ConversationEvent::MessagesAppended { messages: vec![text_message(3, "user", "a user row")] });
        assert_eq!(state.streaming_reply().peek().as_deref(), Some("partial"), "a user row leaves the reply streaming");
        apply_event(state, ConversationEvent::MessagesAppended { messages: vec![text_message(4, "assistant", "partial and done")] });
        assert_eq!(*state.streaming_reply().peek(), None);
    });
}

#[test]
fn test_apply_event_sandbox_pod_update_adds_then_removes_the_pod() {
    with_state(|state| {
        let effect = apply_event(state, ConversationEvent::SandboxPodUpdate { pod_id: 7, status: "running".to_string(), terminated: false });
        assert_eq!(effect, EventEffect::None);
        assert_eq!(state.sandbox_pods().peek().iter().map(|p| (p.pod_id, p.status.clone())).collect::<Vec<_>>(), vec![(7, "running".to_string())]);
        apply_event(state, ConversationEvent::SandboxTerminalUpdate { pod_id: 7, terminal_id: 70, status: "open".to_string(), terminated: false });
        apply_event(state, ConversationEvent::SandboxPodUpdate { pod_id: 7, status: "gone".to_string(), terminated: true });
        assert!(state.sandbox_pods().peek().is_empty());
        assert!(state.sandbox_terminals().peek().is_empty(), "the pod's terminals go with it");
    });
}

#[test]
fn test_apply_event_sandbox_preview_update_sets_the_pods_previews() {
    with_state(|state| {
        apply_event(state, ConversationEvent::SandboxPodUpdate { pod_id: 7, status: "running".to_string(), terminated: false });
        let effect = apply_event(state, ConversationEvent::SandboxPreviewUpdate { pod_id: 7, previews: vec![preview(3000)] });
        assert_eq!(effect, EventEffect::None);
        assert_eq!(state.sandbox_pods().peek()[0].previews, vec![preview(3000)]);
    });
}

/// SME-122 review 1: a turn starting, or a pull that finds no kept error,
/// ends a turn's error but not one only this tab saw (a failed send): the
/// user's message would vanish with no error.
#[test]
fn test_a_later_turn_clears_a_turns_error_but_not_a_failed_send() {
    let mut errors = ChatErrors::default();
    errors.set_turn_error(1, "the turn failed".to_string());
    errors.set_local_error(2, "couldn't send".to_string());
    errors.clear_turn_error(1);
    errors.clear_turn_error(2);
    assert_eq!(errors.lines.get(&1), None);
    assert_eq!(errors.lines.get(&2).map(String::as_str), Some("couldn't send"));

    // A turn's error replaced by a local one stays too.
    errors.set_turn_error(3, "the turn failed".to_string());
    errors.set_local_error(3, "couldn't stop".to_string());
    errors.clear_turn_error(3);
    assert_eq!(errors.lines.get(&3).map(String::as_str), Some("couldn't stop"));
    errors.clear(3);
    assert_eq!(errors.lines.get(&3), None);
}

/// B19 (SME-122): a turn starting says so, so the page drops an earlier
/// turn's error; one ending doesn't.
#[test]
fn test_apply_event_a_turn_starting_clears_an_earlier_error() {
    with_state(|state| {
        assert_eq!(apply_event(state, ConversationEvent::TurnState { running: true, elapsed: 0 }), EventEffect::TurnStarted);
        assert!(*state.turn_running().peek());
        assert_eq!(apply_event(state, ConversationEvent::TurnState { running: false, elapsed: 0 }), EventEffect::None);
    });
}

#[test]
fn test_apply_event_sandbox_terminal_update_adds_then_removes_the_terminal() {
    with_state(|state| {
        let effect = apply_event(state, ConversationEvent::SandboxTerminalUpdate { pod_id: 7, terminal_id: 70, status: "open".to_string(), terminated: false });
        assert_eq!(effect, EventEffect::None);
        assert_eq!(state.sandbox_terminals().peek().iter().map(|t| (t.terminal_id, t.pod_id)).collect::<Vec<_>>(), vec![(70, 7)]);
        apply_event(state, ConversationEvent::SandboxTerminalUpdate { pod_id: 7, terminal_id: 70, status: "closed".to_string(), terminated: true });
        assert!(state.sandbox_terminals().peek().is_empty());
    });
}

#[test]
fn test_apply_event_sandbox_command_update_starts_a_command_then_adds_its_output() {
    with_state(|state| {
        apply_event(state, ConversationEvent::SandboxTerminalUpdate { pod_id: 7, terminal_id: 70, status: "open".to_string(), terminated: false });
        let effect = apply_event(
            state,
            ConversationEvent::SandboxCommandUpdate {
                terminal_id: 70,
                command_id: "c1".to_string(),
                command: Some("seq 1 2".to_string()),
                status: "running".to_string(),
                exit_code: None,
                stream: None,
                latest_output: None,
                position: None,
            },
        );
        assert_eq!(effect, EventEffect::None);
        apply_event(
            state,
            ConversationEvent::SandboxCommandUpdate {
                terminal_id: 70,
                command_id: "c1".to_string(),
                command: None,
                status: "running".to_string(),
                exit_code: None,
                stream: Some("stdout".to_string()),
                latest_output: Some("1".to_string()),
                position: Some(1),
            },
        );
        let terminals = state.sandbox_terminals();
        let terminals = terminals.peek();
        assert_eq!(terminals[0].commands.len(), 1);
        assert_eq!(terminals[0].commands[0].command, "seq 1 2");
        assert_eq!(terminals[0].commands[0].output.iter().map(|l| l.data.as_str()).collect::<Vec<_>>(), vec!["1"]);
    });
}

#[test]
fn test_apply_event_notification_delivery_failed_shows_its_detail() {
    with_state(|state| {
        let effect = apply_event(state, ConversationEvent::NotificationDeliveryFailed { detail: "no model".to_string() });
        assert_eq!(effect, EventEffect::None);
        assert_eq!(state.notification_delivery_error().peek().as_deref(), Some("no model"));
    });
}

#[test]
fn test_apply_event_context_usage_update_sets_the_meter() {
    with_state(|state| {
        let usage = TokenUsage { input_tokens: 1200, ..TokenUsage::default() };
        let effect = apply_event(state, ConversationEvent::ContextUsageUpdate { usage, context_window: 200_000 });
        assert_eq!(effect, EventEffect::None);
        let snapshot = state.context_usage();
        let snapshot = snapshot.peek();
        let snapshot = snapshot.as_ref().expect("a snapshot");
        assert_eq!(snapshot.usage, Some(usage));
        assert_eq!(snapshot.context_window, 200_000);
    });
}

#[test]
fn test_apply_event_todo_list_update_replaces_the_todos() {
    with_state(|state| {
        let items = vec![TodoItem { content: "split the page".to_string(), status: TodoStatus::InProgress }];
        let effect = apply_event(state, ConversationEvent::TodoListUpdate { items: items.clone() });
        assert_eq!(effect, EventEffect::None);
        assert_eq!(*state.todos().peek(), items);
    });
}

#[test]
fn test_apply_event_browsing_session_update_opens_and_a_close_clears_the_frame_and_url() {
    with_state(|state| {
        let effect = apply_event(state, ConversationEvent::BrowsingSessionUpdate { open: true });
        assert_eq!(effect, EventEffect::None);
        assert!(*state.browsing_session_open().peek());
        state.browsing_frame().set(Some("frame".to_string()));
        state.browsing_url().set(Some("https://example.com/".to_string()));
        apply_event(state, ConversationEvent::BrowsingSessionUpdate { open: false });
        assert!(!*state.browsing_session_open().peek());
        assert_eq!(*state.browsing_frame().peek(), None);
        assert_eq!(*state.browsing_url().peek(), None);
    });
}

#[test]
fn test_apply_event_browsing_url_update_sets_the_url() {
    with_state(|state| {
        let effect = apply_event(state, ConversationEvent::BrowsingUrlUpdate { url: "https://example.com/".to_string() });
        assert_eq!(effect, EventEffect::None);
        assert_eq!(state.browsing_url().peek().as_deref(), Some("https://example.com/"));
    });
}

#[test]
fn test_apply_event_repos_update_replaces_the_repos() {
    with_state(|state| {
        let effect = apply_event(state, ConversationEvent::ReposUpdate { repos: vec![repo(1), repo(2)] });
        assert_eq!(effect, EventEffect::None);
        assert_eq!(state.repos().peek().iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 2]);
    });
}

#[test]
fn test_apply_event_question_update_sets_and_clears_the_waiting_question() {
    with_state(|state| {
        let question = PendingQuestion { tool_use_id: "toolu_1".to_string(), questions: vec![] };
        let effect = apply_event(state, ConversationEvent::QuestionUpdate { question: Some(question.clone()) });
        assert_eq!(effect, EventEffect::None);
        assert_eq!(*state.pending_question().peek(), Some(question));
        apply_event(state, ConversationEvent::QuestionUpdate { question: None });
        assert_eq!(*state.pending_question().peek(), None);
    });
}

#[test]
fn test_apply_event_turn_state_sets_running_and_an_ended_turn_drops_its_reply() {
    with_state(|state| {
        let effect = apply_event(state, ConversationEvent::TurnState { running: true, elapsed: 0 });
        assert_eq!(effect, EventEffect::TurnStarted, "B19: an earlier error goes");
        assert!(*state.turn_running().peek());
        assert_eq!(*state.turn_elapsed().peek(), 0);
        // SME-160: the event carries the turn's age on the server's
        // clock, and the state takes it as-is — the timer shows the
        // turn's real age, not this tab's.
        state.turn_elapsed().set(12);
        apply_event(state, ConversationEvent::TurnState { running: true, elapsed: 87 });
        assert_eq!(*state.turn_elapsed().peek(), 87);
        state.streaming_reply().set(Some("half a reply".to_string()));
        apply_event(state, ConversationEvent::TurnState { running: false, elapsed: 0 });
        assert!(!*state.turn_running().peek());
        assert_eq!(*state.turn_elapsed().peek(), 0);
        assert_eq!(*state.streaming_reply().peek(), None);
    });
}

#[test]
fn test_apply_event_reply_reset_then_deltas_build_the_streaming_reply() {
    with_state(|state| {
        assert_eq!(apply_event(state, ConversationEvent::ReplyReset {}), EventEffect::None);
        assert_eq!(state.streaming_reply().peek().as_deref(), Some(""));
        apply_event(state, ConversationEvent::ReplyDelta { text: "Hel".to_string(), offset: 0 });
        assert_eq!(apply_event(state, ConversationEvent::ReplyDelta { text: "lo".to_string(), offset: 3 }), EventEffect::None);
        assert_eq!(state.streaming_reply().peek().as_deref(), Some("Hello"));
    });
}

#[test]
fn test_apply_event_turn_error_is_kept_but_a_stop_is_not() {
    with_state(|state| {
        assert_eq!(
            apply_event(state, ConversationEvent::TurnError { message: "overloaded".to_string() }),
            EventEffect::TurnError("overloaded".to_string())
        );
        assert_eq!(
            apply_event(state, ConversationEvent::TurnError { message: crate::api::chat::TURN_STOPPED.to_string() }),
            EventEffect::None,
            "a stop shows as its saved notice instead"
        );
    });
}

#[test]
fn test_apply_event_counters_and_the_stale_bundle_are_left_to_the_page() {
    with_state(|state| {
        assert_eq!(apply_event(state, ConversationEvent::PodsChanged {}), EventEffect::PodsChanged);
        assert_eq!(apply_event(state, ConversationEvent::TurnsChanged {}), EventEffect::TurnsChanged);
        assert_eq!(apply_event(state, ConversationEvent::QuestionsChanged {}), EventEffect::QuestionsChanged);
        // SME-269: the row a Home tab's list is missing, renamed or sorted.
        assert_eq!(apply_event(state, ConversationEvent::ConversationsChanged {}), EventEffect::ConversationsChanged);
        assert_eq!(apply_event(state, ConversationEvent::ModelChanged {}), EventEffect::ModelChanged);
        assert_eq!(apply_event(state, ConversationEvent::ProvidersChanged {}), EventEffect::ModelChanged);
        assert_eq!(apply_event(state, ConversationEvent::Unknown), EventEffect::StaleBundle);
    });
}
