//! What's known of a model call's usage while it streams (SME-178), so a
//! call that never finishes can still be recorded with what it used.

use std::sync::{Arc, Mutex, MutexGuard};

use super::TokenUsage;
use super::interpret::DeltaUsage;

/// A model call's usage as its stream reports it, shared between the
/// stream loop (which writes it as events arrive) and whoever records
/// the call (which reads it if the call ends early). The loop keeps its
/// usage here only, and `StreamedTurn.usage` is read from it at the end,
/// so the two can't disagree.
///
/// States: empty (nothing known); started (`message_start`'s input and
/// cache counts); streaming (bytes of text, thinking or tool input
/// counted); reported (a `message_delta` gave `output_tokens`). A Stop or
/// a failure ends it in whichever state it reached; `known` says what it
/// knows then.
#[derive(Clone, Debug, Default)]
pub struct UsageMeter(Arc<Mutex<Metered>>);

#[derive(Debug, Default)]
struct Metered {
    usage: TokenUsage,
    output_reported: bool,
    streamed_bytes: u64,
}

impl UsageMeter {
    fn lock(&self) -> MutexGuard<'_, Metered> {
        // Every write is one field assignment: nothing is left half-changed.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `message_start`'s input and cache counts (last one wins). Its
    /// `output_tokens` is a placeholder, and ignored.
    pub(super) fn start(&self, initial: &TokenUsage) {
        let mut metered = self.lock();
        metered.usage.input_tokens = initial.input_tokens;
        metered.usage.cache_creation_input_tokens = initial.cache_creation_input_tokens;
        metered.usage.cache_read_input_tokens = initial.cache_read_input_tokens;
    }

    /// A `message_delta`'s usage, by `DeltaUsage::apply_to`'s rule.
    pub(super) fn delta(&self, delta: &DeltaUsage) {
        let mut metered = self.lock();
        delta.apply_to(&mut metered.usage);
        if delta.output_tokens.is_some() {
            metered.output_reported = true;
        }
    }

    /// `bytes` more of text, thinking or tool input streamed. A thinking
    /// signature isn't output the model wrote, and isn't counted.
    pub(super) fn streamed(&self, bytes: usize) {
        self.lock().streamed_bytes += bytes as u64;
    }

    /// The usage the stream reported, for a call that finished.
    pub(super) fn usage(&self) -> TokenUsage {
        self.lock().usage
    }

    /// What's known of the call's usage, for one that ended before it
    /// finished: `None` when nothing is (no counts and nothing streamed).
    /// Its `output_tokens` is the reported count, or, when none came, an
    /// estimate from the bytes streamed at 4 bytes a token, the ratio
    /// `turn::estimate_tokens` uses: no provider reports output before its
    /// final `message_delta`.
    pub fn known(&self) -> Option<TokenUsage> {
        let metered = self.lock();
        let mut usage = metered.usage;
        if !metered.output_reported {
            usage.output_tokens = i64::try_from(metered.streamed_bytes / 4).unwrap_or(i64::MAX);
        }
        let nothing = metered.streamed_bytes == 0 && usage == TokenUsage::default();
        (!nothing).then_some(usage)
    }
}
