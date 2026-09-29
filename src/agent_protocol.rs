//! The sandbox agent's wire protocol (SME-53): every message smelt and the
//! agent (`src/bin/sandbox_agent.rs`) send each other over the agent's
//! WebSocket, as JSON text frames. Shared by both: the agent compiles this
//! file in with `#[path]`, as it does `docker_net.rs`, so it can use only
//! `std` and `serde`, never `crate::` paths.
//!
//! A pod bakes the agent into its image and outlives a smelt restart or
//! deploy, so the two ends can be built from different commits. The agent
//! says which protocol it speaks in its first message (`hello`), and smelt
//! refuses an agent whose major version differs from its own.

use serde::{Deserialize, Serialize};

/// Which protocol a build speaks. Bump it with every change to the types in
/// this file:
///
/// * **major**, and start a new `fixtures/agent_protocol_v{major}.jsonl`,
///   for a change the other end can't read: a renamed or removed field or
///   variant, a new required field, or a field whose meaning changed. smelt
///   refuses an agent with another major, and the model is told to restart
///   the pod.
/// * **minor** for an addition the other end can ignore: a new action or
///   event, or a new field that is `Option` or `#[serde(default)]`. smelt
///   works with an agent on any minor of its major. A feature an older agent
///   lacks checks `supports(minor)` first, and only that feature fails.
///
/// The fixture test fails on any change to an existing message, which is
/// the reminder.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 1, minor: 0 };

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u32,
    pub minor: u32,
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// smelt to the agent. An action with a `request_id` gets exactly one
/// `AgentMessage::Reply` with that id; `command` and `signal` get none (a
/// command's output and exit arrive as `output`/`exit` events).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ClientMessage {
    CreateTerminal {
        request_id: u64,
        terminal_id: String,
    },
    TerminateTerminal {
        request_id: u64,
        terminal_id: String,
    },
    Command {
        terminal_id: String,
        id: String,
        command: String,
    },
    Signal {
        terminal_id: String,
        id: String,
        signal: String,
    },
    ReadFile {
        request_id: u64,
        path: String,
        offset: u32,
        limit: u32,
    },
    WriteFile {
        request_id: u64,
        path: String,
        content: String,
        expected_hash: Option<String>,
    },
    EditFile {
        request_id: u64,
        path: String,
        old_string: String,
        new_string: String,
        replace_all: bool,
        expected_hash: String,
        expected_line: Option<u32>,
    },
    ListDirectory {
        request_id: u64,
        path: String,
    },
    Glob {
        request_id: u64,
        path: String,
        pattern: String,
        offset: u32,
        limit: u32,
    },
    Grep {
        request_id: u64,
        path: String,
        pattern: String,
        glob: Option<String>,
        case_insensitive: bool,
        offset: u32,
        limit: u32,
    },
}

/// The agent to smelt. `hello` is always the first message on a connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentMessage {
    Hello(ProtocolVersion),
    /// One line a terminal's current command wrote. `seq` counts from 0 for
    /// each command.
    Output {
        id: String,
        terminal_id: String,
        stream: Stream,
        seq: u64,
        data: String,
    },
    Exit {
        id: String,
        terminal_id: String,
        code: i32,
    },
    Reply {
        request_id: u64,
        result: Reply,
    },
    /// A message the agent couldn't parse. `request_id` is there when the
    /// agent could still find one in it, so smelt can fail that request at
    /// once instead of waiting it out.
    ProtocolError {
        request_id: Option<u64>,
        message: String,
    },
    /// An event from an agent on a newer minor. smelt ignores it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    pub fn as_str(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
        }
    }
}

/// The answer to one request. `error` is the agent's own refusal, in words
/// meant for the model (a hash mismatch, an ambiguous edit, an unknown
/// terminal).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reply {
    TerminalCreated,
    TerminalTerminated,
    FileRead(FileContents),
    FileWritten { hash: String },
    FileEdited { hash: String },
    DirectoryListed { entries: Vec<DirEntry> },
    GlobMatched(GlobResult),
    GrepMatched(GrepResult),
    Error { message: String },
    /// A reply kind from an agent on a newer minor.
    #[serde(other)]
    Unknown,
}

/// `read_file`'s page of a file. `hash` is the whole file's content hash,
/// which a later `write_file`/`edit_file` must match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileContents {
    pub lines: Vec<String>,
    pub total_lines: usize,
    pub hash: String,
}

/// One `list_directory` entry — `size` is only meaningful for a file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
}

/// One `grep` match — see SME-19.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GrepMatch {
    pub path: String,
    pub line: u32,
    pub text: String,
}

/// One file `grep` excluded (too large, or not valid UTF-8) — reported
/// explicitly rather than silently dropped, per SME-19's "Decisions from
/// review."
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkippedFile {
    pub path: String,
    pub reason: String,
}

/// `glob`'s result. `total` is the match count the walk actually found, up
/// to the agent's scan ceiling (`MAX_GLOB_SCAN`) — not just how many fit in
/// this page. `scan_capped` is true only when that ceiling itself was hit,
/// distinct from ordinary pagination (`total` larger than one page,
/// `scan_capped: false`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlobResult {
    pub paths: Vec<String>,
    pub total: usize,
    pub scan_capped: bool,
}

/// `grep`'s result — same `total`/`scan_capped` meaning as `GlobResult`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GrepResult {
    pub matches: Vec<GrepMatch>,
    pub total: usize,
    pub scan_capped: bool,
    pub skipped: Vec<SkippedFile>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// One example of every message at major 1, one JSON object per line.
    const FIXTURE_V1: &str = include_str!("fixtures/agent_protocol_v1.jsonl");

    /// Every variant's tag. Exhaustive matches, so a new variant doesn't
    /// compile until it's named here, and then the fixture needs an example
    /// of it.
    fn client_tag(message: &ClientMessage) -> &'static str {
        match message {
            ClientMessage::CreateTerminal { .. } => "create_terminal",
            ClientMessage::TerminateTerminal { .. } => "terminate_terminal",
            ClientMessage::Command { .. } => "command",
            ClientMessage::Signal { .. } => "signal",
            ClientMessage::ReadFile { .. } => "read_file",
            ClientMessage::WriteFile { .. } => "write_file",
            ClientMessage::EditFile { .. } => "edit_file",
            ClientMessage::ListDirectory { .. } => "list_directory",
            ClientMessage::Glob { .. } => "glob",
            ClientMessage::Grep { .. } => "grep",
        }
    }

    fn agent_tag(message: &AgentMessage) -> String {
        match message {
            AgentMessage::Hello(_) => "hello".into(),
            AgentMessage::Output { .. } => "output".into(),
            AgentMessage::Exit { .. } => "exit".into(),
            AgentMessage::Reply { result, .. } => format!("reply/{}", reply_tag(result)),
            AgentMessage::ProtocolError { .. } => "protocol_error".into(),
            AgentMessage::Unknown => "unknown".into(),
        }
    }

    fn reply_tag(reply: &Reply) -> &'static str {
        match reply {
            Reply::TerminalCreated => "terminal_created",
            Reply::TerminalTerminated => "terminal_terminated",
            Reply::FileRead(_) => "file_read",
            Reply::FileWritten { .. } => "file_written",
            Reply::FileEdited { .. } => "file_edited",
            Reply::DirectoryListed { .. } => "directory_listed",
            Reply::GlobMatched(_) => "glob_matched",
            Reply::GrepMatched(_) => "grep_matched",
            Reply::Error { .. } => "error",
            Reply::Unknown => "unknown",
        }
    }

    const EVERY_CLIENT_TAG: &[&str] = &[
        "create_terminal",
        "terminate_terminal",
        "command",
        "signal",
        "read_file",
        "write_file",
        "edit_file",
        "list_directory",
        "glob",
        "grep",
    ];

    /// `unknown` is only ever parsed, never sent, so it has no example.
    const EVERY_AGENT_TAG: &[&str] = &[
        "hello",
        "output",
        "exit",
        "reply/terminal_created",
        "reply/terminal_terminated",
        "reply/file_read",
        "reply/file_written",
        "reply/file_edited",
        "reply/directory_listed",
        "reply/glob_matched",
        "reply/grep_matched",
        "reply/error",
        "protocol_error",
    ];

    #[test]
    fn test_every_v1_fixture_message_parses_and_serializes_to_the_same_bytes() {
        let mut client_tags = BTreeSet::new();
        let mut agent_tags = BTreeSet::new();
        for line in FIXTURE_V1.lines().filter(|l| !l.trim().is_empty()) {
            if line.starts_with("{\"action\"") {
                let message: ClientMessage = serde_json::from_str(line)
                    .unwrap_or_else(|e| panic!("fixture line doesn't parse ({e}): {line}"));
                assert_eq!(serde_json::to_string(&message).expect("serializes"), line);
                client_tags.insert(client_tag(&message).to_string());
            } else {
                let message: AgentMessage = serde_json::from_str(line)
                    .unwrap_or_else(|e| panic!("fixture line doesn't parse ({e}): {line}"));
                assert_eq!(serde_json::to_string(&message).expect("serializes"), line);
                agent_tags.insert(agent_tag(&message));
            }
        }
        let expected_client: BTreeSet<String> = EVERY_CLIENT_TAG.iter().map(|t| t.to_string()).collect();
        let expected_agent: BTreeSet<String> = EVERY_AGENT_TAG.iter().map(|t| t.to_string()).collect();
        assert_eq!(client_tags, expected_client, "the fixture needs one example of every client message");
        assert_eq!(agent_tags, expected_agent, "the fixture needs one example of every agent message");
    }

    #[test]
    fn test_an_unknown_event_from_a_newer_agent_parses_as_unknown() {
        let message: AgentMessage =
            serde_json::from_str(r#"{"event":"terminal_resized","terminal_id":"1","rows":40}"#)
                .expect("an unknown event still parses");
        assert_eq!(message, AgentMessage::Unknown);
    }

    #[test]
    fn test_an_unknown_reply_kind_from_a_newer_agent_parses_as_unknown() {
        let message: AgentMessage =
            serde_json::from_str(r#"{"event":"reply","request_id":7,"result":{"kind":"file_moved","to":"b"}}"#)
                .expect("an unknown reply kind still parses");
        assert_eq!(message, AgentMessage::Reply { request_id: 7, result: Reply::Unknown });
    }

    #[test]
    fn test_an_unknown_field_from_a_newer_agent_is_ignored() {
        let message: AgentMessage =
            serde_json::from_str(r#"{"event":"exit","id":"c1","terminal_id":"1","code":0,"duration_ms":12}"#)
                .expect("an extra field is ignored");
        assert_eq!(
            message,
            AgentMessage::Exit { id: "c1".into(), terminal_id: "1".into(), code: 0 }
        );
    }

    /// The first thing a protocol-0 agent (before SME-53) sends is an
    /// untagged output line. It must not pass for a hello, or smelt would
    /// take an old agent for a current one.
    #[test]
    fn test_a_protocol_0_output_line_is_not_a_hello() {
        let v0_line = r#"{"id":"cmd-1","terminal_id":"3","stream":"stdout","seq":0,"data":"hi"}"#;
        let parsed = serde_json::from_str::<AgentMessage>(v0_line);
        assert!(
            !matches!(parsed, Ok(AgentMessage::Hello(_))),
            "a protocol-0 line parsed as a hello: {parsed:?}"
        );
    }
}
