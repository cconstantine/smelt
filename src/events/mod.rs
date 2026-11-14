//! Per-conversation live event bus — the shared home for pushing updates
//! that happen with no `send_message` request in flight (a command's
//! output or completion), so neither `anthropic::tools` nor `api::chat`
//! has to depend on the other to publish or read these. The tools call
//! `publish`; `chat.rs` calls both `publish` (after persisting a batch of
//! rows) and `subscribe` (to relay everything to a browser tab).

use serde::{Deserialize, Serialize};

use crate::anthropic::TokenUsage;
use crate::models::Message;

/// A port in a sandbox pod the user can open in their own browser, and the
/// preview address that reaches it (`preview::PreviewTemplate::url_for`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SandboxPreview {
    pub port: u16,
    /// A Docker container's address in the pod, or `None` for the pod's
    /// own localhost (SME-33).
    #[serde(default)]
    pub host: Option<String>,
    pub url: String,
}

impl SandboxPreview {
    /// What the preview reaches, as the panel names it: `port 3000`, or a
    /// container's `172.21.0.2:3000`.
    pub fn label(&self) -> String {
        match &self.host {
            Some(host) => format!("{host}:{}", self.port),
            None => format!("port {}", self.port),
        }
    }
}

/// What a conversation has been waiting on the user for since it last
/// went busy (SME-71): published to the notifier, and passed back to
/// `api::notifications::get_attention` as the kind to re-check. Ordered
/// by priority — during one busy spell a question outranks a failure, a
/// failure outranks the model's own `notify_user` (`Notified`, SME-377),
/// so a later turn can't hide an earlier one. A turn ending notes nothing
/// by itself: `Notified` replaced the automatic `Finished`, and an older
/// tab that can't read it reloads (`check_build_id`). Serde as a string.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum Attention {
    Notified,
    CutOff,
    Refused,
    Failed,
    Question,
}

/// `MessagesAppended` carries no new data of its own; it's a live-delivery
/// notification for rows `db::create_message` already persisted. The
/// `Sandbox*` variants are ephemeral UI telemetry, never persisted and
/// regenerable at any time from `api::sandbox::get_sandbox_state` — see
/// SME-10.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ConversationEvent {
    /// A struct variant, not `MessagesAppended(Vec<Message>)`: this enum
    /// is internally tagged, and serde can't write a tuple variant holding
    /// a list that way — every send failed, silently.
    MessagesAppended { messages: Vec<Message> },
    /// Published once on `create_pod` (already `Running` by the time it
    /// returns) and once on `terminate_pod`.
    SandboxPodUpdate {
        pod_id: i64,
        status: String,
        /// The node it runs on, once scheduled (SME-304). Defaulted, so a
        /// tab from before it still reads the event.
        #[serde(default)]
        node: Option<String>,
        terminated: bool,
    },
    /// Published when the model shares a preview (`sandbox_preview_url`),
    /// carrying every preview that pod has so far — the panel replaces its
    /// list with this one. See SME-42.
    SandboxPreviewUpdate {
        pod_id: i64,
        previews: Vec<SandboxPreview>,
    },
    /// Published once on `create_terminal`, once on `terminate_terminal`,
    /// and once per terminal a crash-cleanup pass clears.
    SandboxTerminalUpdate {
        pod_id: i64,
        terminal_id: i64,
        status: String,
        terminated: bool,
    },
    /// One variant covering "started", "one new output line", and
    /// "finished", distinguished by which optional fields are set. `stream`
    /// is `"stdout"`/`"stderr"`, or `None` for a pure status transition. Deliberately doesn't carry `pod_id`: the
    /// frontend already knows a terminal's pod from `SandboxTerminalUpdate`,
    /// so a command update only ever needs to find an already-known
    /// terminal by `terminal_id`. `command` is only `Some` on the "started"
    /// event (published by `run_terminal_command_tool`, which has the
    /// command text in hand) — the output-line/finished events publish from
    /// `sandbox.rs`'s `handle_agent_message`, which only ever sees
    /// `command_id`, not the command it was for, and shouldn't pay for a DB
    /// lookup per output line just to repeat it.
    SandboxCommandUpdate {
        terminal_id: i64,
        command_id: String,
        command: Option<String>,
        status: String,
        exit_code: Option<i32>,
        stream: Option<String>,
        latest_output: Option<String>,
        /// The agent's `seq` for `latest_output`, one counter per command
        /// across both streams. A tab that reconnects skips a line its
        /// snapshot already has (SME-51 B3). `None` when there's no line.
        position: Option<i64>,
    },
    /// A terminal command's script MCP calls so far (SME-359), published
    /// as they finish, at most every 500 ms per command (the last one
    /// always). A tab that doesn't know the command skips it.
    SandboxMcpCalls {
        command_id: String,
        summary: crate::api::mcp_calls::McpCallSummary,
    },
    /// Published when a `turn::notify` wake or delivery (fired when a terminal
    /// command finishes, to notify the model with no further tool call
    /// needed) fails to actually reach the model — e.g. no model provider
    /// set up, a transient Anthropic API error. The underlying notification
    /// text is still durably persisted regardless (`wake_conversation`
    /// drains and persists it *before* the API call that might fail) — this
    /// means "the model hasn't been prompted with it yet," not "it's lost."
    /// See SME-13.
    NotificationDeliveryFailed {
        detail: String,
    },
    /// Published after every real turn completes — ephemeral UI telemetry,
    /// same category as `Sandbox*Update`, regenerable at any
    /// time from `api::chat::get_context_usage`. See
    /// SME-18.
    ContextUsageUpdate {
        usage: TokenUsage,
        context_window: u32,
    },
    /// Published by `todowrite` on every call — carries the *complete*
    /// list (never a partial diff, matching `todowrite`'s own whole-list-
    /// replace semantics), so the frontend panel can just overwrite its
    /// signal wholesale rather than merging line by line.
    /// Ephemeral UI telemetry, regenerable at any time from
    /// `db::get_conversation_todos`. See
    /// SME-20.
    TodoListUpdate {
        items: Vec<crate::anthropic::tools::TodoItem>,
    },
    /// Published by `open_browser_session`/`close_browser_session` — the
    /// live panel uses this to show/hide reactively rather than only
    /// checking on conversation (re)select, same reasoning
    /// `SandboxPodUpdate` already established for the sandbox panel.
    /// Ephemeral UI telemetry, regenerable at any time from
    /// `api::browsing::get_browsing_state`. See
    /// SME-22.
    BrowsingSessionUpdate {
        open: bool,
    },
    /// Published whenever a browsing session's page URL changes — the model
    /// navigating, a link click, a redirect, or an in-page change like
    /// `pushState` — so the live panel's address bar can follow along.
    /// Regenerable from `api::browsing::get_browsing_state`.
    BrowsingUrlUpdate {
        url: String,
    },
    /// The conversation's repos (SME-32), all of them, published whenever
    /// one is added, starts cloning, or finishes. Regenerable from
    /// `api::git::list_conversation_repos`.
    ReposUpdate {
        repos: Vec<crate::git::RepoSummary>,
    },
    /// An app-wide `AppEvent::PodsChanged` (a pod was created or went
    /// away, in any conversation), relayed on every conversation's stream
    /// so a chat tab doesn't need a second always-open connection for the
    /// sidebar's pod dots: browsers allow only 6 connections per host over
    /// HTTP/1.1, shared across tabs.
    PodsChanged {},
    /// An app-wide `AppEvent::TurnsChanged` (a turn started or ended in
    /// some conversation), relayed like `PodsChanged` so the sidebar can
    /// mark which conversations are busy (SME-41 D9).
    TurnsChanged {},
    /// The question this conversation waits on (SME-34): the model asked
    /// one (`Some`), or it was answered or dismissed (`None`). Regenerable
    /// from `api::questions::get_pending_question`.
    QuestionUpdate {
        question: Option<crate::questions::PendingQuestion>,
    },
    /// An app-wide `AppEvent::QuestionsChanged`, relayed like `TurnsChanged`
    /// so the sidebar can mark conversations waiting on an answer.
    QuestionsChanged {},
    /// A conversation needs the user (SME-71): `AppEvent::NeedsUser`,
    /// relayed like `TurnsChanged` so every chat tab keeps its title
    /// count. A chat tab hears every conversation's.
    NeedsUser {
        conversation_id: i64,
        attention: Attention,
        generation: u64,
    },
    /// An app-wide `AppEvent::ConversationsChanged` (a conversation was
    /// created, retitled or deleted), relayed like `TurnsChanged` so the
    /// sidebar's list follows along in every tab (SME-269).
    ConversationsChanged {},
    /// The conversation's provider or model changed (the user chose
    /// another, or a turn took the default). Carries nothing: the tab
    /// refetches `api::chat::get_conversation_model` (SME-72).
    ModelChanged {},
    /// An app-wide `AppEvent::ProvidersChanged` (a provider or the default
    /// model was added, edited or removed), relayed like `PodsChanged` so
    /// the model picker and the no-provider notice stay current (SME-72).
    ProvidersChanged {},
    /// An app-wide `AppEvent::CommandsChanged` (a terminal command started
    /// or finished somewhere), relayed like `PodsChanged` so the sidebar
    /// marks conversations whose commands still run (SME-441).
    CommandsChanged {},
    /// Whether a model turn is running (or queued) in this conversation,
    /// published when that changes, so every tab watching it can offer a
    /// Stop button, including for turns it didn't start. `elapsed` is how
    /// long the busy spell had been running on the server's clock when
    /// this was published, in whole seconds: 0 at both transitions
    /// (SME-160). Regenerable, with the turn's real age, from
    /// `api::chat::get_turn_state`.
    TurnState {
        running: bool,
        elapsed: u64,
    },
    /// A model call is starting: whatever reply text a tab is showing as
    /// "streaming" is done with (it's saved, or it was a false start the
    /// retry replaces). Each model call gets its own streaming bubble.
    ReplyReset {},
    /// A Claude Code call of this conversation's waits at the five-hour
    /// usage gate (SME-523) until `until`, in Unix seconds; `None` once
    /// it no longer does. Regenerable from `api::usage_windows::get_usage_hold`.
    UsageHold {
        until: Option<i64>,
    },
    /// More reply text as the model streams it. Published for every
    /// turn, whoever started it, so every tab watching sees the reply
    /// arrive live. Regenerable mid-turn from
    /// `api::chat::get_reply_in_progress`.
    ReplyDelta {
        text: String,
        /// The reply's length in bytes before `text`, so a tab that
        /// fetched the reply so far skips what it already has (SME-51 B3).
        offset: usize,
    },
    /// A turn the user started by sending a message failed or was
    /// stopped (`message` is then `api::chat::TURN_STOPPED`). Turns
    /// started by a finished command or task report failure as
    /// `NotificationDeliveryFailed` instead.
    TurnError {
        message: String,
    },
    /// A type the browser tier's web bundle doesn't have (`dx build` leaves
    /// this feature off), standing in for one a newer server adds.
    #[cfg(feature = "browser-test")]
    BrowserTestAddedLater {},
    /// An event type this build doesn't know: one added on a newer server
    /// than the page's bundle. Never published; a tab skips it and offers
    /// a reload (SME-43).
    #[serde(other)]
    Unknown,
}

/// Events that aren't about one conversation, for views that span them
/// all: the sidebar and the pods view. `subscribe_app_events` relays them.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum AppEvent {
    /// A pod was created or went away (stopped, crashed, or torn down with
    /// its conversation), or a conversation's stored sandbox volumes were
    /// removed (SME-253). Carries nothing: listeners refetch.
    PodsChanged,
    /// A model turn started or ended in some conversation. Carries
    /// nothing: listeners refetch `api::agents::get_conversation_tree`,
    /// whose busy marks and agents' statuses it changes.
    TurnsChanged,
    /// A conversation started or stopped waiting on the user: an answer to
    /// the model's question (SME-34), or an AGENTS.md trust card, published
    /// with any change to its repos (SME-246). Carries nothing: listeners
    /// refetch `api::agents::get_conversation_tree` and the open
    /// conversation's `get_agents_waiting`.
    QuestionsChanged,
    /// A conversation went idle with something for the user (SME-71):
    /// published once, after `TurnsChanged`, when its last turn ended and
    /// it had noted attention: a question asked, a failure, a cut-off or
    /// refused reply, or the model's own `notify_user` (SME-377). A turn
    /// ending notes nothing by itself.
    /// `generation` is the conversation's turn generation at that end;
    /// `api::notifications::get_attention` answers `None` for a stale
    /// event, and the notifier checks it right before acting.
    NeedsUser {
        conversation_id: i64,
        attention: Attention,
        generation: u64,
    },
    /// A conversation was created, retitled (its first message gave it a
    /// title) or deleted, or an agent's stored state changed. Carries
    /// nothing: listeners refetch `api::agents::get_conversation_tree`,
    /// which is the list, its titles, its order and its agents (SME-269,
    /// SME-246).
    ConversationsChanged,
    /// A model provider or the default model was added, edited or
    /// removed. Carries nothing: listeners refetch (SME-72).
    ProvidersChanged,
    /// A terminal command started or finished somewhere (SME-441): the
    /// sidebar's command markers. Carries nothing: listeners refetch
    /// `api::pods::get_running_command_conversations`.
    CommandsChanged,
    /// As `ConversationEvent::Unknown`.
    #[serde(other)]
    Unknown,
}


#[cfg(feature = "server")]
mod server;
#[cfg(test)]
mod wire_tests;

#[cfg(feature = "server")]
pub use server::{Subscription, forget, publish, publish_app, subscribe, subscribe_app};
#[cfg(all(feature = "server", test))]
pub use server::{AppRecording, has_channel, subscriber_count};
