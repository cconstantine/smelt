//! Result and line caps, and the edit result that survives them (SME-76).

use super::*;

#[test]
fn test_cap_tool_result_cuts_a_long_result_and_says_so() {
    let (capped, cut) = cap_tool_result("x".repeat(40_000));
    assert!(cut, "it says it cut");
    assert!(capped.starts_with(&"x".repeat(MAX_TOOL_RESULT_CHARS)), "the start is kept");
    assert!(!capped.contains(&"x".repeat(MAX_TOOL_RESULT_CHARS + 1)), "the rest is dropped");
    assert!(
        capped.contains("cut to the first 30000 of 40000 characters"),
        "the note says how much was cut: {}",
        &capped[MAX_TOOL_RESULT_CHARS..]
    );
}

#[test]
fn test_cap_tool_result_leaves_a_result_at_the_cap_alone() {
    let whole = "é".repeat(MAX_TOOL_RESULT_CHARS);
    assert_eq!(cap_tool_result(whole.clone()), (whole, false));
}

/// The cap is in `execute`, so it covers every tool and errors too.
/// An unknown tool's error echoes its name, the cheapest long
/// result that needs no pod or server.
#[tokio::test]
async fn test_execute_caps_an_error_too() {
    let name = "n".repeat(40_000);
    let (result, spans) =
        crate::telemetry::capture_spans(execute(&test_pool(), 1, "t1", &name, &serde_json::json!({}))).await;
    let message = result.expect_err("an unknown tool is an error");
    assert!(message.chars().count() < MAX_TOOL_RESULT_CHARS + 200, "{} chars", message.chars().count());
    let [span] = spans.as_slice() else { panic!("one span: {spans:?}") };
    assert_eq!(span_attribute(span, "smelt.tool.truncated"), Some(&true.into()), "its span says it was cut");
}

#[test]
fn test_fit_lines_cuts_a_long_line_and_stops_at_the_budget() {
    let mut lines = vec!["z".repeat(5_000)];
    lines.extend((0..40).map(|_| "y".repeat(1_500)));
    let (kept, truncated) = fit_lines(lines, LINES_BUDGET_CHARS, 0);
    assert!(truncated, "41 lines of 1.5–5k chars don't fit 25k");
    assert!(kept[0].starts_with(&"z".repeat(MAX_LINE_CHARS)), "{}", &kept[0][..20]);
    assert!(kept[0].ends_with("[line cut at 2000 chars]"), "{}", &kept[0][MAX_LINE_CHARS..]);
    let encoded: usize = kept.iter().map(|l| serde_json::to_string(l).map_or(0, |s| s.len())).sum();
    assert!(encoded <= LINES_BUDGET_CHARS, "{encoded} chars kept");
    assert!(kept.len() > 10, "it fills the budget, kept {}", kept.len());
}

#[test]
fn test_fit_lines_keeps_one_line_even_over_the_budget() {
    let (kept, truncated) = fit_lines(vec!["a".repeat(100), "b".to_string()], 10, 0);
    assert_eq!(kept, vec!["a".repeat(100)]);
    assert!(truncated);
}

/// SME-76 review: the cap in `execute` cut an edit's result inside
/// its diagnostics, which serialize before `hash`, so the new hash
/// was lost and the next edit was refused as a concurrent change.
#[test]
fn test_an_edit_result_with_huge_diagnostics_keeps_its_hash_and_stays_json() {
    let diagnostics = (0..50)
        .map(|i| format!("error[E0308] at {i}:1  {}", "x".repeat(1_000)))
        .collect::<Vec<_>>()
        .join("\n");
    let (result, _) = cap_tool_result(edit_result_json("new-hash".to_string(), Some(diagnostics)));
    let parsed: Value = serde_json::from_str(&result).expect("the result is still JSON");
    assert_eq!(parsed["hash"], "new-hash");
    let shown = parsed["diagnostics"].as_str().expect("diagnostics");
    assert!(shown.starts_with("error[E0308] at 0:1"), "the first diagnostics are kept");
    assert!(shown.contains("cut"), "it says the diagnostics were cut");
}

#[test]
fn test_an_edit_result_with_short_diagnostics_is_unchanged() {
    let result = edit_result_json("h".to_string(), Some("no errors".to_string()));
    assert_eq!(result, r#"{"diagnostics":"no errors","hash":"h"}"#);
}

