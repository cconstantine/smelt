//! Sending a run what the turn added since its last call (SME-430): the
//! reply's results to its parked calls, or a user message, and answering
//! the calls the CLI makes as they come.

use std::collections::HashMap;


use super::*;

impl Run {
    /// Sends what the turn added since the last call: the reply's results
    /// to its parked calls, or a user message.
    pub(super) async fn send_tail(&mut self, tail: &[AnthropicMessage]) -> Result<(), String> {
        match self.state {
            State::Streaming => Err("Claude Code was still replying when smelt called it again.".to_string()),
            State::Idle => {
                let names = call_names(tail);
                let text = tail.iter().map(|m| blocks_text(&m.content, &names)).collect::<Vec<_>>().join("\n\n");
                self.write_user(&text).await
            }
            State::Parked => {
                let mut replies: HashMap<String, ToolReply> = HashMap::new();
                let mut others = Vec::new();
                for block in tail.iter().flat_map(|m| &m.content) {
                    match block {
                        ContentBlock::ToolResult { tool_use_id, content, is_error }
                            if self.expected.iter().any(|e| &e.id == tool_use_id) =>
                        {
                            let reply = ToolReply { text: vec![content.clone()], is_error: is_error.unwrap_or(false) };
                            replies.insert(tool_use_id.clone(), reply);
                        }
                        ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                            others.push(tool_result_text(tool_use_id, None, content, is_error.unwrap_or(false)));
                        }
                        other => others.push(blocks_text(std::slice::from_ref(other), &HashMap::new())),
                    }
                }
                others.retain(|text| !text.trim().is_empty());
                for expected in &self.expected {
                    replies
                        .entry(expected.id.clone())
                        .or_insert_with(|| ToolReply { text: vec![NOT_RUN.to_string()], is_error: true });
                }
                if let Some(last) = self.expected.last().and_then(|e| replies.get_mut(&e.id)) {
                    last.text.extend(others);
                }
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
                self.state = State::Streaming;
                Ok(())
            }
        }
    }

    /// A call from the CLI: answered now if smelt has its result, else
    /// kept until it does. One without a tool-use id is matched to a call
    /// of the reply by name and input, first come first served.
    /// The calls held while the reply streamed, at its `message_stop`:
    /// each one of its calls waits for smelt's result; any other is
    /// refused (review 2 L12).
    pub(super) fn settle_held(&mut self) {
        for parked in std::mem::take(&mut self.held) {
            let ours = parked.tool_use_id.as_ref().is_some_and(|id| self.expected.iter().any(|e| &e.id == id));
            match (ours, parked.tool_use_id.clone()) {
                (true, Some(id)) => {
                    self.arrived.insert(id, parked);
                }
                _ => {
                    let _ = parked.reply.send(ToolReply { text: vec![NOT_THE_REPLYS.to_string()], is_error: true });
                }
            }
        }
    }

    pub(super) fn take_call(&mut self, parked: ParkedCall) {
        let id = parked.tool_use_id.clone().or_else(|| {
            self.expected
                .iter()
                .find(|e| e.name == parked.name && e.input == parked.arguments && self.answers.contains_key(&e.id))
                .map(|e| e.id.clone())
        });
        let refuse = |parked: ParkedCall| {
            let _ = parked.reply.send(ToolReply { text: vec![NOT_THE_REPLYS.to_string()], is_error: true });
        };
        match id {
            Some(id) => match self.answers.remove(&id) {
                Some(reply) => {
                    let _ = parked.reply.send(reply);
                    self.answered.insert(id);
                }
                // Not answered yet: the CLI makes a reply's first call
                // before its `message_stop` (S2), and smelt runs it first.
                None if self.expected.iter().any(|e| e.id == id) || self.streaming_ids.contains(&id) => {
                    self.arrived.insert(id, parked);
                }
                // While a reply streams, its own first call can reach the
                // bridge before smelt has read its block, or even its start,
                // from stdout: held until the reply's `message_stop` (review
                // 2 L12), on a loop's later replies too (PR #285 review 1
                // L16). Only while some of the last reply's results wait to
                // be taken is the CLI still making that reply's calls, so an
                // unknown id then is a stray, refused at once (L1).
                None if self.state == State::Streaming && self.answers.is_empty() => self.held.push(parked),
                // Not one of the reply's calls: refused at once (review 1 L1).
                None => refuse(parked),
            },
            // No id, and its input may not have streamed yet: it waits for
            // the reply's results, and is refused once they're in.
            None if !self.delivered => self.anonymous.push(parked),
            None => refuse(parked),
        }
    }
}
