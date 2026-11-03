#[test]
fn test_a_preview_label_names_the_port_or_the_container() {
    let preview = |host: Option<&str>| SandboxPreview {
        port: 3000,
        host: host.map(str::to_string),
        url: String::new(),
    };
    assert_eq!(preview(None).label(), "port 3000");
    assert_eq!(preview(Some("172.21.0.2")).label(), "172.21.0.2:3000");
}

use super::*;

#[test]
fn test_app_events_round_trip_through_json() {
    let event = AppEvent::PodsChanged;
    let json = serde_json::to_string(&event).expect("serialize");
    assert_eq!(json, r#"{"type":"PodsChanged"}"#);
    assert_eq!(serde_json::from_str::<AppEvent>(&json).expect("deserialize"), event);
    for event in [
        AppEvent::TurnsChanged,
        AppEvent::ProvidersChanged,
        AppEvent::QuestionsChanged,
        AppEvent::ConversationsChanged,
        AppEvent::NeedsUser { conversation_id: 7, attention: Attention::Failed, generation: 12 },
    ] {
        let json = serde_json::to_string(&event).expect("serialize");
        assert_eq!(serde_json::from_str::<AppEvent>(&json).expect("deserialize"), event);
    }
}

/// SME-269: an older bundle must be able to skip it, so it goes on the wire
/// as a tagged object like its neighbours — a unit variant written any other
/// way decodes as `Unknown` there rather than failing loudly.
#[test]
fn test_a_list_change_event_round_trips_through_json() {
    let app = AppEvent::ConversationsChanged;
    assert_eq!(serde_json::to_string(&app).expect("serialize"), r#"{"type":"ConversationsChanged"}"#);
    assert_eq!(serde_json::from_str::<AppEvent>(r#"{"type":"ConversationsChanged"}"#).expect("app event"), app);
    let conversation = ConversationEvent::ConversationsChanged {};
    assert_eq!(
        serde_json::to_string(&conversation).expect("serialize"),
        r#"{"type":"ConversationsChanged"}"#
    );
    assert_eq!(
        serde_json::from_str::<ConversationEvent>(r#"{"type":"ConversationsChanged"}"#)
            .expect("conversation event"),
        conversation
    );
}

/// A tab still running an older bundle gets event types added since.
/// It skips them rather than taking the stream for broken (SME-43).
#[test]
fn test_an_event_type_added_later_decodes_as_unknown() {
    for json in [r#"{"type":"AddedLater"}"#, r#"{"type":"AddedLater","x":[1,2],"y":{"z":"w"}}"#] {
        assert_eq!(
            serde_json::from_str::<ConversationEvent>(json).expect("conversation event"),
            ConversationEvent::Unknown,
            "{json}"
        );
        assert_eq!(
            serde_json::from_str::<AppEvent>(json).expect("app event"),
            AppEvent::Unknown,
            "{json}"
        );
    }
}

/// SME-71: the notifier passes the kind it was told back to
/// `get_attention`, so `Attention` travels as a plain string — and the
/// server's "keep the maximum" relies on this exact priority order:
/// a question outranks a failure, a failure outranks a finished turn.
#[test]
fn test_attention_is_a_string_on_the_wire_and_orders_by_priority() {
    for (attention, name) in [
        (Attention::Finished, "Finished"),
        (Attention::CutOff, "CutOff"),
        (Attention::Refused, "Refused"),
        (Attention::Failed, "Failed"),
        (Attention::Question, "Question"),
    ] {
        let json = format!("\"{name}\"");
        assert_eq!(serde_json::to_string(&attention).expect("serialize"), json);
        assert_eq!(serde_json::from_str::<Attention>(&json).expect("deserialize"), attention);
    }
    let by_priority = [Attention::Finished, Attention::CutOff, Attention::Refused, Attention::Failed, Attention::Question];
    for pair in by_priority.windows(2) {
        assert!(pair[0] < pair[1], "{:?} should rank below {:?}", pair[0], pair[1]);
    }
}

/// SME-71: the exact shape the leader and the chat tabs' counts read.
#[test]
fn test_a_needs_user_event_names_its_conversation_kind_and_generation() {
    let json = serde_json::to_string(&AppEvent::NeedsUser {
        conversation_id: 7,
        attention: Attention::CutOff,
        generation: 4,
    })
    .expect("serialize");
    assert_eq!(json, r#"{"type":"NeedsUser","conversation_id":7,"attention":"CutOff","generation":4}"#);
    let back: AppEvent = serde_json::from_str(&json).expect("app event");
    assert_eq!(back, AppEvent::NeedsUser { conversation_id: 7, attention: Attention::CutOff, generation: 4 });
    let json = serde_json::to_string(&ConversationEvent::NeedsUser {
        conversation_id: 7,
        attention: Attention::CutOff,
        generation: 4,
    })
    .expect("serialize");
    assert_eq!(json, r#"{"type":"NeedsUser","conversation_id":7,"attention":"CutOff","generation":4}"#);
}

fn one_of_each() -> Vec<ConversationEvent> {
    vec![
        ConversationEvent::TurnError { message: "boom".to_string() },
        ConversationEvent::ReplyReset {},
        ConversationEvent::ReplyDelta { text: "Hi".to_string(), offset: 0 },
        ConversationEvent::TurnState { running: true, elapsed: 0 },
        ConversationEvent::PodsChanged {},
        ConversationEvent::TurnsChanged {},
        ConversationEvent::QuestionsChanged {},
        ConversationEvent::NeedsUser {
            conversation_id: 7,
            attention: Attention::Question,
            generation: 3,
        },
        ConversationEvent::ConversationsChanged {},
        ConversationEvent::QuestionUpdate { question: None },
        ConversationEvent::QuestionUpdate {
            question: Some(crate::questions::PendingQuestion {
                tool_use_id: "toolu_q".to_string(),
                questions: vec![crate::questions::Question {
                    question: "Delete it?".to_string(),
                    header: "Delete".to_string(),
                    options: vec![
                        crate::questions::QuestionOption { label: "Yes".to_string(), description: None },
                        crate::questions::QuestionOption { label: "No".to_string(), description: Some("keep it".to_string()) },
                    ],
                    multi_select: false,
                }],
            }),
        },
        ConversationEvent::ModelChanged {},
        ConversationEvent::ProvidersChanged {},
        ConversationEvent::MessagesAppended { messages: vec![Message {
            id: 7,
            conversation_id: 3,
            role: "assistant".to_string(),
            content: r#"[{"type":"text","text":"hi"}]"#.to_string(),
            created_at: chrono::DateTime::from_timestamp(1_790_000_000, 0)
                .expect("valid timestamp")
                .naive_utc(),
        }] },
        ConversationEvent::SandboxPodUpdate {
            pod_id: 1,
            status: "running".to_string(), node: None,
            terminated: false,
        },
        ConversationEvent::SandboxPreviewUpdate {
            pod_id: 1,
            previews: vec![
                SandboxPreview {
                    port: 3000,
                    host: None,
                    url: "http://3000-3.preview.localhost:8181".to_string(),
                },
                SandboxPreview {
                    port: 3000,
                    host: Some("172.21.0.2".to_string()),
                    url: "http://172-21-0-2-3000-3.preview.localhost:8181".to_string(),
                },
            ],
        },
        ConversationEvent::SandboxTerminalUpdate {
            pod_id: 1,
            terminal_id: 2,
            status: "connected".to_string(),
            terminated: false,
        },
        ConversationEvent::SandboxCommandUpdate {
            terminal_id: 2,
            command_id: "c1".to_string(),
            command: Some("ls".to_string()),
            status: "running".to_string(),
            exit_code: None,
            stream: None,
            latest_output: None,
            position: None,
        },
        ConversationEvent::NotificationDeliveryFailed {
            detail: "boom".to_string(),
        },
        ConversationEvent::ContextUsageUpdate {
            usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 2,
                cache_creation_input_tokens: 3,
                cache_read_input_tokens: 4,
            },
            context_window: 200_000,
        },
        ConversationEvent::TodoListUpdate {
            items: vec![crate::anthropic::tools::TodoItem {
                content: "ship it".to_string(),
                status: crate::anthropic::tools::TodoStatus::Pending,
            }],
        },
        ConversationEvent::BrowsingSessionUpdate { open: true },
        ConversationEvent::BrowsingUrlUpdate {
            url: "https://example.com/".to_string(),
        },
        ConversationEvent::ReposUpdate {
            repos: vec![crate::git::RepoSummary {
                id: 1,
                url: "git@github.com:o/r.git".to_string(),
                path: "/workspace/r".to_string(),
                requested_branch: Some("dev".to_string()),
                branch: Some("dev".to_string()),
                commit: Some("abc123".to_string()),
                status: crate::git::RepoStatus::Failed,
                error: Some("fatal: nope".to_string()),
                agents_files: vec![],
                loaded_instructions: vec![],
                trust_requests: vec![],
            }],
        },
    ]
}

/// Every event goes to the browser as JSON, and a variant serde can't
/// write is silently dropped on the way — so every variant must
/// survive a round trip.
#[test]
fn test_every_event_round_trips_through_json() {
    for event in one_of_each() {
        let json = serde_json::to_string(&event)
            .unwrap_or_else(|e| panic!("{event:?} doesn't serialize: {e}"));
        let back: ConversationEvent = serde_json::from_str(&json)
            .unwrap_or_else(|e| panic!("{json} doesn't deserialize: {e}"));
        assert_eq!(back, event);
    }
}
