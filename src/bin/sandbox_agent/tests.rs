use super::*;

/// SME-122 review 1: the last command's marker turning up late (its
/// stderr was slow past the fallback) is dropped, and doesn't count as
/// the next command's: that one still waits for its own.
/// Whether `found` (from `Ending::find`) stands for the in-flight
/// command.
fn counts_for_current(found: &Option<(String, EndMarker)>) -> bool {
    matches!(found, Some((_, EndMarker::Out(_) | EndMarker::Err(_))))
}

#[test]
fn test_a_late_marker_of_the_last_command_changes_nothing() {
    let marker = std::sync::Mutex::new(Some("M_A:".to_string()));
    let mut ending = Ending::default();
    let (_, out) = ending.find(&marker, "M_A:o:0").expect("stdout's marker");
    ending.note(&out);
    ending.give_up_waiting(); // stderr's didn't come in time
    assert_eq!(ending.complete(&marker), Some(0));

    *marker.lock().unwrap_or_else(|e| e.into_inner()) = Some("M_B:".to_string());
    let late = ending.find(&marker, "M_A:e:0");
    assert!(late.is_some(), "the late marker is recognized, so it isn't shown");
    assert!(!counts_for_current(&late), "it doesn't stand for the current command's marker");
}

#[test]
fn test_split_marker_finds_a_marker_alone_on_its_line() {
    assert_eq!(split_marker("__smelt_done__:n1:o:0", "__smelt_done__:n1:"), Some(("", 'o', 0)));
    assert_eq!(split_marker("__smelt_done__:n1:e:130", "__smelt_done__:n1:"), Some(("", 'e', 130)));
}

#[test]
fn test_split_marker_finds_a_marker_glued_to_output_without_a_newline() {
    // Conversation 23's `head -c 400` of a JSON body.
    assert_eq!(
        split_marker(r#""conclusion":"succes__smelt_done__:n1:o:0"#, "__smelt_done__:n1:"),
        Some((r#""conclusion":"succes"#, 'o', 0))
    );
}

#[test]
fn test_split_marker_ignores_another_commands_nonce_and_the_old_marker() {
    assert_eq!(split_marker("__smelt_done__:n2:o:0", "__smelt_done__:n1:"), None);
    assert_eq!(split_marker("MARKER:0", "__smelt_done__:n1:"), None);
    assert_eq!(split_marker("__smelt_done__:n1:", "__smelt_done__:n1:"), None);
    assert_eq!(split_marker("__smelt_done__:n1:o:0 trailing", "__smelt_done__:n1:"), None);
    assert_eq!(split_marker("__smelt_done__:n1:x:0", "__smelt_done__:n1:"), None);
    assert_eq!(split_marker("__smelt_done__:n1:0", "__smelt_done__:n1:"), None, "the untagged form");
}

#[test]
fn test_relay_target_accepts_a_container_address_and_port() {
    assert_eq!(
        relay_target("172.21.0.2:3000"),
        Ok("172.21.0.2:3000".parse().unwrap())
    );
}

#[test]
fn test_relay_target_refuses_anything_outside_the_docker_range() {
    for line in [
        "10.43.0.1:443",
        "192.168.8.130:6443",
        "127.0.0.1:8088",
        "localhost:3000",
        "172.21.0.2",
        "172.21.0.2:0",
        "172.21.0.2:99999",
        "",
    ] {
        assert!(relay_target(line).is_err(), "{line:?} should be refused");
    }
}

#[test]
fn test_hash_content_returns_sha256_hex_digest() {
    // Known SHA-256("hello") test vector.
    assert_eq!(
        hash_content(b"hello"),
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}

#[test]
fn test_find_matches_returns_every_non_overlapping_occurrence() {
    assert_eq!(find_matches("abcabcabc", "abc"), vec![0, 3, 6]);
}

#[test]
fn test_find_matches_none_found() {
    assert_eq!(find_matches("hello world", "xyz"), Vec::<usize>::new());
}

#[test]
fn test_find_matches_does_not_double_count_overlapping_windows() {
    // "aa" against "aaa": ordinary (non-overlapping) substring-replace
    // semantics report one match at offset 0, not two.
    assert_eq!(find_matches("aaa", "aa"), vec![0]);
}

#[test]
fn test_byte_offset_to_line_first_line_is_one() {
    assert_eq!(byte_offset_to_line("hello\nworld", 0), 1);
}

#[test]
fn test_byte_offset_to_line_counts_preceding_newlines() {
    let text = "line1\nline2\nline3";
    let offset_of_line3 = text.find("line3").unwrap();
    assert_eq!(byte_offset_to_line(text, offset_of_line3), 3);
}

#[test]
fn test_byte_offset_to_line_mid_line_offset_still_counts_as_that_line() {
    let text = "line1\nline2\nline3";
    // Points into the middle of "line2", not its start.
    let mid_line2 = text.find("line2").unwrap() + 2;
    assert_eq!(byte_offset_to_line(text, mid_line2), 2);
}

#[test]
fn test_apply_edit_replaces_a_unique_match() {
    let result =
        apply_edit("hello world", "world", "there", false, None).expect("should succeed");
    assert_eq!(result, "hello there");
}

#[test]
fn test_apply_edit_errors_when_old_string_not_found() {
    let result = apply_edit("hello world", "xyz", "there", false, None);
    assert_eq!(result, Err(EditError::NotFound));
}

#[test]
fn test_apply_edit_errors_when_ambiguous() {
    let result = apply_edit("foo foo foo", "foo", "bar", false, None);
    assert_eq!(result, Err(EditError::Ambiguous { count: 3 }));
}

#[test]
fn test_apply_edit_replace_all_replaces_every_occurrence() {
    let result = apply_edit("foo foo foo", "foo", "bar", true, None).expect("should succeed");
    assert_eq!(result, "bar bar bar");
}

#[test]
fn test_apply_edit_expected_line_targets_one_occurrence_among_duplicates() {
    // Two identical lines — expected_line picks the second one
    // specifically, leaving the first untouched.
    let content = "let x = 1;\nlet x = 1;\n";
    let line2_start = content.match_indices("let x = 1;").nth(1).unwrap().0;
    let expected_line = byte_offset_to_line(content, line2_start);
    let result = apply_edit(
        content,
        "let x = 1;",
        "let x = 2;",
        false,
        Some(expected_line),
    )
    .expect("should succeed");
    assert_eq!(result, "let x = 1;\nlet x = 2;\n");
}

#[test]
fn test_apply_edit_expected_line_errors_when_no_match_starts_there() {
    let content = "let x = 1;\nlet y = 2;\n";
    let result = apply_edit(content, "let x = 1;", "let x = 2;", false, Some(99));
    assert_eq!(result, Err(EditError::NoMatchAtLine { line: 99 }));
}

#[test]
fn test_apply_edit_multiline_old_and_new_string() {
    let content = "fn f() {\n    old_body();\n}\n";
    let result = apply_edit(
        content,
        "fn f() {\n    old_body();\n}",
        "fn f() {\n    new_body();\n    more();\n}",
        false,
        None,
    )
    .expect("should succeed");
    assert_eq!(result, "fn f() {\n    new_body();\n    more();\n}\n");
}

#[test]
fn test_paginate_lines_returns_full_content_within_limit() {
    let (lines, total) = paginate_lines("a\nb\nc", 1, 10);
    assert_eq!(lines, vec!["a", "b", "c"]);
    assert_eq!(total, 3);
}

#[test]
fn test_paginate_lines_respects_offset() {
    let (lines, total) = paginate_lines("a\nb\nc", 2, 10);
    assert_eq!(lines, vec!["b", "c"]);
    assert_eq!(total, 3);
}

#[test]
fn test_paginate_lines_respects_limit() {
    let (lines, total) = paginate_lines("a\nb\nc", 1, 2);
    assert_eq!(lines, vec!["a", "b"]);
    assert_eq!(
        total, 3,
        "total should reflect the whole file, not just the returned slice"
    );
}

#[test]
fn test_paginate_lines_offset_beyond_end_returns_empty_but_correct_total() {
    let (lines, total) = paginate_lines("a\nb\nc", 99, 10);
    assert!(lines.is_empty());
    assert_eq!(total, 3);
}

// --- paginate_slice: same offset/limit semantics as paginate_lines,
// but over an already-materialized Vec<T> (glob's Vec<String> paths,
// grep's Vec<GrepMatch>) rather than splitting `&str` into lines —
// see SME-19.

#[test]
fn test_paginate_slice_returns_full_slice_within_limit() {
    let (page, total) = paginate_slice(&["a", "b", "c"], 1, 10);
    assert_eq!(page, vec!["a", "b", "c"]);
    assert_eq!(total, 3);
}

#[test]
fn test_paginate_slice_respects_offset() {
    let (page, total) = paginate_slice(&["a", "b", "c"], 2, 10);
    assert_eq!(page, vec!["b", "c"]);
    assert_eq!(total, 3);
}

#[test]
fn test_paginate_slice_respects_limit() {
    let (page, total) = paginate_slice(&["a", "b", "c"], 1, 2);
    assert_eq!(page, vec!["a", "b"]);
    assert_eq!(
        total, 3,
        "total should reflect the whole slice, not just the returned page"
    );
}

#[test]
fn test_paginate_slice_offset_beyond_end_returns_empty_but_correct_total() {
    let (page, total): (Vec<&str>, usize) = paginate_slice(&["a", "b", "c"], 99, 10);
    assert!(page.is_empty());
    assert_eq!(total, 3);
}

// --- compile_glob_pattern: shared by glob's own `pattern` and grep's
// optional `glob` filter — see SME-19's
// "Decisions from review."

#[test]
fn test_compile_glob_pattern_matches_double_star_extension() {
    let matcher = compile_glob_pattern("**/*.rs").expect("valid pattern");
    assert!(matcher.is_match("src/main.rs"));
    assert!(matcher.is_match("main.rs"));
    assert!(!matcher.is_match("src/main.py"));
}

#[test]
fn test_compile_glob_pattern_plain_star_does_not_cross_directories() {
    let matcher = compile_glob_pattern("*.rs").expect("valid pattern");
    assert!(matcher.is_match("main.rs"));
    assert!(
        !matcher.is_match("src/main.rs"),
        "a bare * shouldn't match across a path separator"
    );
}

#[test]
fn test_compile_glob_pattern_rejects_invalid_pattern() {
    let err = compile_glob_pattern("[").expect_err("unterminated character class");
    assert!(
        err.contains("invalid glob pattern"),
        "expected a clear invalid-pattern error, got: {err}"
    );
}

// --- compile_grep_pattern / grep_content: grep's own regex-matching
// core, deliberately separated from the real directory walk (which is
// only exercised by the real-cluster integration test) — see
// SME-19.

#[test]
fn test_compile_grep_pattern_rejects_invalid_regex() {
    let err = compile_grep_pattern("(", false).expect_err("unbalanced group");
    assert!(
        err.contains("invalid regex pattern"),
        "expected a clear invalid-pattern error, got: {err}"
    );
}

#[test]
fn test_compile_grep_pattern_case_insensitive_matches_regardless_of_case() {
    let pattern = compile_grep_pattern("hello", true).expect("valid pattern");
    assert!(pattern.is_match("HELLO world"));
}

#[test]
fn test_compile_grep_pattern_case_sensitive_by_default() {
    let pattern = compile_grep_pattern("hello", false).expect("valid pattern");
    assert!(!pattern.is_match("HELLO world"));
}

#[test]
fn test_grep_content_returns_every_matching_line_with_1_indexed_line_numbers() {
    let pattern = compile_grep_pattern("fn ", false).expect("valid pattern");
    let (matches, budget_exhausted) =
        grep_content("fn a() {}\nlet x = 1;\nfn b() {}\n", &pattern, 10);
    assert_eq!(
        matches,
        vec![(1, "fn a() {}".to_string()), (3, "fn b() {}".to_string())]
    );
    assert!(!budget_exhausted);
}

#[test]
fn test_grep_content_no_matches_returns_empty() {
    let pattern = compile_grep_pattern("nope", false).expect("valid pattern");
    let (matches, budget_exhausted) = grep_content("a\nb\nc\n", &pattern, 10);
    assert!(matches.is_empty());
    assert!(!budget_exhausted);
}

#[test]
fn test_grep_content_stops_at_budget_and_reports_exhaustion() {
    let pattern = compile_grep_pattern("x", false).expect("valid pattern");
    let (matches, budget_exhausted) = grep_content("x\nx\nx\nx\n", &pattern, 2);
    assert_eq!(matches, vec![(1, "x".to_string()), (2, "x".to_string())]);
    assert!(
        budget_exhausted,
        "should report that more matches existed than the budget allowed"
    );
}

fn entry(name: &str, is_dir: bool, size: Option<u64>) -> DirEntry {
    DirEntry {
        name: name.to_string(),
        is_dir,
        size,
    }
}

#[test]
fn test_sort_entries_orders_alphabetically_case_sensitive() {
    let entries = vec![
        entry("banana", false, Some(3)),
        entry("Apple", true, None),
        entry("cherry", false, Some(5)),
    ];
    let sorted = sort_entries(entries);
    assert_eq!(
        sorted.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
        // Byte-wise ordering: uppercase 'A' (0x41) sorts before
        // lowercase 'b'/'c' (0x62/0x63).
        vec!["Apple", "banana", "cherry"]
    );
}

#[test]
fn test_edit_error_message_not_found() {
    assert_eq!(
        edit_error_message(EditError::NotFound),
        "old_string not found in file"
    );
}

#[test]
fn test_edit_error_message_ambiguous_includes_match_count() {
    let message = edit_error_message(EditError::Ambiguous { count: 4 });
    assert!(
        message.contains('4'),
        "expected the match count in the message, got: {message}"
    );
}

#[test]
fn test_edit_error_message_no_match_at_line_includes_the_line_number() {
    let message = edit_error_message(EditError::NoMatchAtLine { line: 42 });
    assert!(
        message.contains("42"),
        "expected the line number in the message, got: {message}"
    );
}
