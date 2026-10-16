use super::*;

/// SME-76: conversation 23 read a 73 KB source file whole.
#[test]
fn test_read_file_response_stops_at_the_budget_and_says_where_to_continue() {
    let mut lines = vec!["z".repeat(5_000)];
    lines.extend((0..60).map(|i| format!("{i:02}{}", "y".repeat(1_000))));
    let contents = crate::agent_protocol::FileContents { lines, total_lines: 100, hash: "h".into() };
    let response = read_file_response(contents, 11);
    assert_eq!(response["truncated"], true);
    let content = response["content"].as_str().expect("content");
    let shown = content.lines().count() as u64;
    assert_eq!(response["next_offset"].as_u64(), Some(11 + shown), "{shown} lines shown");
    assert!(content.lines().next().expect("a line").ends_with("[line cut at 2000 chars]"));
    // `MAX_TOOL_RESULT_CHARS` in `tools::server`, which `files` can't see
    assert!(response.to_string().chars().count() < 30_000);
    assert_eq!(response["hash"], "h", "the hash is still the whole file's");
}

#[test]
fn test_read_file_response_gives_a_next_offset_when_the_file_goes_on() {
    let contents = crate::agent_protocol::FileContents {
        lines: vec!["a".into(), "b".into()],
        total_lines: 5,
        hash: "h".into(),
    };
    let response = read_file_response(contents, 1);
    assert_eq!(response["next_offset"], 3);
    assert!(response.get("truncated").is_none(), "nothing was left out for size");
    let whole = crate::agent_protocol::FileContents { lines: vec!["a".into()], total_lines: 1, hash: "h".into() };
    assert!(read_file_response(whole, 1).get("next_offset").is_none(), "the file ends here");
}
