//! Closing a run whose turn returned while its reply's calls waited
//! (SME-444): each call is answered (its real result, or a question's
//! placeholder, a success), the CLI's echo of each is awaited, and only
//! then is the run interrupted. An interrupt before the echo would have
//! the CLI record the call as stopped (P3), which the model reads as a
//! refusal.
//!
//! Closing lives only inside the ending task (`Run::ending`):
//! - all answered and echoed, or all answered and the CLI's next API
//!   call starting (`status: requesting`, `message_start`): interrupt,
//!   then the usual wind-down;
//! - a `result` first (the CLI ended by itself), or the process gone:
//!   the wind-down without an interrupt;
//! - `CLOSE_WAIT` passing first: a warning naming the calls not echoed,
//!   then the interrupt, as a run that wasn't closed gets.
//!
//! Whichever way it ends, the calls the CLI echoed, questions aside, are
//! noted as delivered (`session::note_delivered`): the next turn's resume
//! doesn't send their results again as text.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use super::*;

/// The longest a close waits for the CLI to echo its answers.
pub const CLOSE_WAIT: Duration = Duration::from_secs(5);

#[cfg(test)]
pub static CLOSE_WAIT_FOR_TESTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn close_wait() -> Duration {
    #[cfg(test)]
    {
        let ms = CLOSE_WAIT_FOR_TESTS.load(std::sync::atomic::Ordering::SeqCst);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    CLOSE_WAIT
}

/// The reply each of `expected`'s calls gets when its turn returns: its
/// result in `held`, or, for a question with none, the placeholder that
/// says where its answer comes from. `None` when another call has no
/// result: that run isn't closed, and its calls read as stopped.
pub(in crate::claude_cli) fn replies(expected: &[Expected], held: &[ContentBlock]) -> Option<HashMap<String, ToolReply>> {
    let mut replies = HashMap::new();
    for call in expected {
        let result = held.iter().find_map(|block| match block {
            ContentBlock::ToolResult { tool_use_id, content, is_error } if *tool_use_id == call.id => {
                Some(ToolReply { text: vec![content.clone()], is_error: is_error.unwrap_or(false) })
            }
            _ => None,
        });
        let placeholder = |text: &str| ToolReply { text: vec![text.to_string()], is_error: false };
        let reply = match result {
            Some(reply) => reply,
            None if call.name == crate::questions::ASK_USER => placeholder(ASKED_USER),
            None if call.name == crate::agents::ASK_PARENT => placeholder(ASKED_PARENT),
            None => return None,
        };
        replies.insert(call.id.clone(), reply);
    }
    Some(replies)
}

/// How a close's wait ended.
enum Settled {
    /// Every call answered and echoed, or the next API call starting.
    Taken,
    /// The CLI ended its run itself, or its process went.
    Ended,
    /// `CLOSE_WAIT` passed.
    TimedOut,
}

impl Run {
    /// Answers every call with `replies` and waits, within `CLOSE_WAIT`,
    /// for the CLI to take them. Whether the run still needs an interrupt.
    pub(super) async fn settle(&mut self, replies: HashMap<String, ToolReply>) -> bool {
        self.answers = replies;
        self.delivered = true;
        for parked in std::mem::take(&mut self.anonymous) {
            self.take_call(parked);
        }
        let arrived: Vec<String> = self.arrived.keys().cloned().collect();
        for id in arrived {
            if let Some(parked) = self.arrived.remove(&id) {
                self.take_call(parked);
            }
        }
        let ids: Vec<String> = self.expected.iter().map(|e| e.id.clone()).collect();
        let mut echoed: HashSet<String> = HashSet::new();
        let waited = tokio::time::timeout(close_wait(), async {
            loop {
                if ids.iter().all(|id| self.answered.contains(id) && echoed.contains(id)) {
                    return Settled::Taken;
                }
                let Ok(event) = self.next_event().await else {
                    return Settled::Ended;
                };
                let all_answered = ids.iter().all(|id| self.answered.contains(id));
                match event["type"].as_str() {
                    Some("user") => {
                        for block in event["message"]["content"].as_array().into_iter().flatten() {
                            if block["type"] == "tool_result"
                                && let Some(id) = block["tool_use_id"].as_str()
                            {
                                echoed.insert(id.to_string());
                            }
                        }
                    }
                    Some("system") if all_answered && event["subtype"] == "status" && event["status"] == "requesting" => return Settled::Taken,
                    Some("stream_event") if all_answered && event["event"]["type"] == "message_start" => return Settled::Taken,
                    Some("result") => return Settled::Ended,
                    _ => self.note(&event),
                }
            }
        })
        .await
        .unwrap_or(Settled::TimedOut);
        // The session has what it echoed, so the next turn doesn't repeat
        // it (Q2); a question's answer is still to come, and isn't marked.
        let delivered: Vec<String> = self
            .expected
            .iter()
            .filter(|e| echoed.contains(&e.id) && e.name != crate::questions::ASK_USER && e.name != crate::agents::ASK_PARENT)
            .map(|e| e.id.clone())
            .collect();
        if !delivered.is_empty() {
            crate::claude_cli::session::note_delivered(self.conversation_id, delivered);
        }
        match waited {
            Settled::Taken => true,
            Settled::Ended => {
                self.state = State::Idle;
                false
            }
            Settled::TimedOut => {
                let waiting: Vec<&String> = ids.iter().filter(|id| !echoed.contains(*id)).collect();
                tracing::warn!(conversation_id = self.conversation_id, ?waiting, "Claude Code didn't echo smelt's answers in time: interrupting its run");
                true
            }
        }
    }
}
