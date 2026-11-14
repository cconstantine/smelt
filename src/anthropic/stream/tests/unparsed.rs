//! SME-520: a call whose input isn't one JSON value stays in the reply
//! with input `{}` and is listed, instead of failing the reply; its
//! input's shape and bounded edges are logged.

use super::super::unparsed::{Shape, edges};
use super::*;
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `future`'s output and what it logged at warn and above, uncoloured.
async fn logged<T>(future: impl std::future::Future<Output = T>) -> (T, String) {
    use tracing::instrument::WithSubscriber;
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let output = future.with_subscriber(subscriber).await;
    let text = String::from_utf8(captured.0.lock().expect("log buffer").clone()).expect("utf-8 log");
    (output, text)
}

fn empty_call(id: &str) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.to_string(),
        name: "add".to_string(),
        input: serde_json::json!({}),
    }
}

/// A reply whose first call's input is `input`, then a good call.
fn broken_then_good(input: &str) -> &'static str {
    let [start_a, input_a] = tool_at(0, "call_a", input);
    let [start_b, input_b] = tool_at(1, "call_b", r#"{"a":2}"#);
    let [delta, stop] = tool_use_end();
    sse(&[start_a, input_a, stop_at(0), start_b, input_b, stop_at(1), delta, stop])
}

/// Test 1: two joined objects keep the reply; the call is kept with
/// `{}` in its place and listed with serde's message.
#[tokio::test]
async fn test_two_joined_objects_keep_the_reply_and_list_the_call() {
    let turn = run_against_mock_upstream(broken_then_good(r#"{"a":1}{"a":1}"#), |_| {})
        .await
        .expect("an unparsed call doesn't fail the reply");
    assert_eq!(turn.content, vec![empty_call("call_a"), add_call("call_b", 2)]);
    assert_eq!(turn.unparsed_calls.len(), 1, "{:?}", turn.unparsed_calls);
    let call = &turn.unparsed_calls[0];
    assert_eq!((call.id.as_str(), call.name.as_str()), ("call_a", "add"));
    assert!(call.error.contains("trailing characters"), "{}", call.error);
    assert!(!call.error.contains("add"), "serde's message alone: {}", call.error);
    assert_eq!(turn.cut_off_call, None);
}

/// Test 2 (was `test_an_unparseable_tool_input_still_fails_the_call`):
/// a stray tail and a truncated input in a finished reply, whether the
/// call's stop came or the stream just ended, are kept the same way.
#[tokio::test]
async fn test_a_stray_tail_or_truncated_input_keeps_the_reply() {
    for (raw, kind) in [(r#"{"a":1}x"#, "trailing characters"), (r#"{"a":"#, "EOF while parsing")] {
        for with_stop in [true, false] {
            let [start, input] = tool_at(0, "call_a", raw);
            let [delta, stop] = tool_use_end();
            let mut events = vec![start, input];
            if with_stop {
                events.push(stop_at(0));
            }
            events.extend([delta, stop]);
            let turn = run_against_mock_upstream(sse(&events), |_| {})
                .await
                .expect("an unparsed call doesn't fail the reply");
            assert_eq!(turn.content, vec![empty_call("call_a")], "{raw} {with_stop}");
            assert_eq!(turn.unparsed_calls.len(), 1, "{raw} {with_stop}");
            assert!(turn.unparsed_calls[0].error.contains(kind), "{}", turn.unparsed_calls[0].error);
        }
    }
}

/// Test 4: the log line's shape fields tell joined objects from a
/// stray tail.
#[tokio::test]
async fn test_the_log_line_tells_the_inputs_shape() {
    let (turn, log) = logged(run_against_mock_upstream(broken_then_good(r#"{"a":1}{"a":1}"#), |_| {})).await;
    turn.expect("an unparsed call doesn't fail the reply");
    let line = log.lines().find(|l| l.contains("tool_use_id=\"call_a\"") || l.contains("tool_use_id=call_a")).unwrap_or_else(|| panic!("a line for call_a in {log}"));
    for part in ["len=14", "values=2", "first_value_len=7", "repeated=true", "tool=\"add\"", "trailing characters"] {
        assert!(line.contains(part), "{part} in {line}");
    }
    assert!(!log.contains("call_b"), "only the broken call is logged: {log}");

    let (turn, log) = logged(run_against_mock_upstream(broken_then_good(r#"{"a":1}x"#), |_| {})).await;
    turn.expect("an unparsed call doesn't fail the reply");
    assert!(log.contains("values=1") && log.contains("repeated=false"), "{log}");
}

/// Test 4: a long input is logged by its length and edges only; a
/// marker in its middle never reaches the log.
#[tokio::test]
async fn test_a_long_input_is_logged_only_by_its_edges() {
    let marker = "MIDDLE-MARKER-5f3a";
    let half = "x".repeat(5_120 - 6 - marker.len() / 2);
    let mut raw = format!(r#"{{"a":"{half}{marker}{half}"#);
    raw.push_str(&"y".repeat(10_240 - 2 - raw.len()));
    raw.push_str("}x");
    assert_eq!(raw.len(), 10_240);
    let raw: &'static str = Box::leak(raw.into_boxed_str());
    let (turn, log) = logged(run_against_mock_upstream(broken_then_good(raw), |_| {})).await;
    let turn = turn.expect("an unparsed call doesn't fail the reply");
    assert_eq!(turn.unparsed_calls.len(), 1);
    assert!(log.contains("len=10240"), "{log}");
    assert!(!log.contains(marker), "the middle stays out of the log");
    assert!(log.len() < 1_000, "{} bytes logged", log.len());
}

#[test]
fn test_edges_cut_at_a_character_boundary_and_show_a_short_input_whole() {
    assert_eq!(edges("{}x"), ("{}x", ""));
    let raw = format!("{}{}", "é".repeat(200), "z");
    let (prefix, suffix) = edges(&raw);
    assert!(prefix.len() <= 160 && prefix.len() >= 159, "{}", prefix.len());
    assert!(suffix.len() <= 160 && suffix.ends_with('z'), "{}", suffix.len());
}

#[test]
fn test_shape_counts_values_until_the_first_error() {
    assert_eq!(Shape::of(r#"{"a":1}{"a":2}"#), Shape { values: 2, first_value_len: Some(7), repeated: false });
    assert_eq!(Shape::of(r#"{"a":"#), Shape { values: 0, first_value_len: None, repeated: false });
}
