use super::*;

#[test]
fn marker_candidates_stop_at_the_workspace() {
    let markers = vec!["Cargo.toml".to_string(), ".git".to_string()];
    assert_eq!(
        marker_candidates("/workspace/app/src/main.rs", &markers),
        vec![
            "/workspace/app/src/Cargo.toml",
            "/workspace/app/src/.git",
            "/workspace/app/Cargo.toml",
            "/workspace/app/.git",
            "/workspace/Cargo.toml",
            "/workspace/.git",
        ]
    );
    assert!(marker_candidates("/etc/passwd", &markers).is_empty());
}

#[test]
fn an_empty_answer_while_indexing_says_so() {
    assert_eq!(
        note_indexing("No results.".to_string(), true, "rust-analyzer"),
        "No results.\n(rust-analyzer is still indexing; try again shortly.)"
    );
    assert_eq!(note_indexing("No results.".to_string(), false, "rust-analyzer"), "No results.");
    assert_eq!(note_indexing("/workspace/a.rs:1:1  fn a()".to_string(), true, "rust-analyzer"), "/workspace/a.rs:1:1  fn a()");
}

#[test]
fn lines_and_characters_count_from_one() {
    let at = |line, character| OperationInput { path: Some("/workspace/a.rs".to_string()), line, character, ..Default::default() };
    assert_eq!(check_input(Operation::Definition, &at(Some(1), Some(1))), Ok(()));
    assert_eq!(check_input(Operation::Definition, &at(Some(1), None)), Ok(()));
    let zero_line = check_input(Operation::Definition, &at(Some(0), Some(1))).expect_err("line 0");
    assert!(zero_line.contains("1-based"), "{zero_line}");
    assert!(check_input(Operation::Hover, &at(Some(3), Some(0))).is_err(), "character 0");
    assert!(check_input(Operation::References, &at(None, None)).is_err(), "no line");
    // Operations without a position don't look at them.
    assert_eq!(check_input(Operation::WorkspaceSymbols, &OperationInput::default()), Ok(()));
}

#[test]
fn a_partial_rename_says_what_was_and_was_not_changed() {
    let message = partial_rename_error(
        "sum_areas",
        &["/workspace/a.rs".to_string()],
        "/workspace/b.rs",
        "/workspace/b.rs changed while it was being renamed",
        &["/workspace/c.rs".to_string()],
    );
    assert_eq!(
        message,
        "The rename to sum_areas stopped partway: /workspace/b.rs changed while it was being renamed.\n\
             Already changed: /workspace/a.rs\n\
             Not changed: /workspace/b.rs, /workspace/c.rs\n\
             Read those files again before renaming the rest or undoing it."
    );
    assert_eq!(
        partial_rename_error("x", &[], "/workspace/a.rs", "/workspace/a.rs changed while it was being renamed", &[]),
        "The rename to x was refused: /workspace/a.rs changed while it was being renamed. Nothing was changed."
    );
}

#[test]
fn a_file_over_the_limit_is_refused_rather_than_cut_short() {
    assert!(within_limit("/workspace/a.rs", "a".repeat(MAX_FILE_BYTES)).is_ok());
    let Err(error) = within_limit("/workspace/big.rs", "a".repeat(MAX_FILE_BYTES + 1)) else {
        panic!("a file over the limit should be refused");
    };
    assert!(error.contains("/workspace/big.rs") && error.contains("1 MiB"), "{error}");
}

#[test]
fn paths_in_finds_each_file_once() {
    let result = json!([
        {"uri": "file:///workspace/a.rs", "range": {}},
        {"targetUri": "file:///workspace/b.rs", "targetRange": {}},
        {"uri": "file:///workspace/a.rs", "range": {}},
        {"location": {"uri": "file:///workspace/c.rs"}},
    ]);
    let mut paths = Vec::new();
    paths_in(&result, &mut paths);
    assert_eq!(paths, vec!["/workspace/a.rs", "/workspace/b.rs", "/workspace/c.rs"]);
}
