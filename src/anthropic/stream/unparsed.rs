//! A tool call whose streamed input isn't one JSON value (SME-520): it
//! stays in the reply with input `{}`, is never run, and its result is
//! an error the model can retry from. This module logs what the input
//! looked like, within bounds, and words that result.

use serde_json::Value;

/// A reply's call whose input didn't parse: listed in
/// `StreamedTurn::unparsed_calls`, its block kept with input `{}`.
#[derive(Debug, Clone, PartialEq)]
pub struct UnparsedCall {
    pub id: String,
    pub name: String,
    /// serde's message alone (`trailing characters at line 1 column 8`):
    /// a position and a kind, never the input.
    pub error: String,
}

/// The most of a broken input's start, and of its end, the log line
/// shows; an input of at most twice this is shown whole.
pub const EDGE: usize = 160;

/// What a broken input is made of: how many whole JSON values it starts
/// with, where the first ends, and whether the first two are the same
/// (a replayed call's input appended to itself).
#[derive(Debug, PartialEq)]
pub(super) struct Shape {
    pub(super) values: usize,
    pub(super) first_value_len: Option<usize>,
    pub(super) repeated: bool,
}

impl Shape {
    pub(super) fn of(raw: &str) -> Self {
        let mut stream = serde_json::Deserializer::from_str(raw).into_iter::<Value>();
        let (mut values, mut first_value_len, mut first, mut repeated) = (0, None, None, false);
        while let Some(Ok(value)) = stream.next() {
            values += 1;
            match values {
                1 => {
                    first_value_len = Some(stream.byte_offset());
                    first = Some(value);
                }
                2 => repeated = first.as_ref() == Some(&value),
                _ => {}
            }
        }
        Self { values, first_value_len, repeated }
    }
}

/// The input's first and last `EDGE` bytes, each cut at a character
/// boundary; the whole input (and no suffix) when it's `2 * EDGE` or less.
pub(super) fn edges(raw: &str) -> (&str, &str) {
    if raw.len() <= 2 * EDGE {
        return (raw, "");
    }
    let mut head = EDGE;
    while !raw.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = raw.len() - EDGE;
    while !raw.is_char_boundary(tail) {
        tail += 1;
    }
    (&raw[..head], &raw[tail..])
}

/// Logs one unparsed call: its length and shape first, then the bounded
/// edges of its input, escaped (`{:?}`), never the whole of a long one.
pub(super) fn report(name: &str, id: &str, raw: &str, error: &str) {
    let shape = Shape::of(raw);
    let (prefix, suffix) = edges(raw);
    tracing::warn!(
        tool = name,
        tool_use_id = id,
        len = raw.len(),
        error,
        values = shape.values,
        first_value_len = shape.first_value_len,
        repeated = shape.repeated,
        prefix = ?prefix,
        suffix = ?suffix,
        "a tool call's input isn't one JSON value; the call goes back to the model as an error"
    );
}
