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

/// SME-172: open files are compared with the disk by hash; a changed one
/// is synced again and a deleted one closed, each matched to its own line.
#[test]
fn stale_files_resyncs_the_changed_and_closes_the_deleted() {
    let open = |files: &[(&str, &str)]| -> Vec<(String, String)> {
        files.iter().map(|(p, t)| (p.to_string(), t.to_string())).collect()
    };
    let files = open(&[("/workspace/a.rs", "fn a() {}"), ("/workspace/b.rs", "fn b() {}"), ("/workspace/c.rs", "fn c() {}")]);
    // Every file as the server has it: nothing to do.
    let same: Vec<String> = files.iter().map(|(_, t)| sha256(t)).collect();
    assert_eq!(stale_files(&files, &same), Some((vec![], vec![])));
    // b changed, c is gone.
    let hashes = vec![sha256("fn a() {}"), sha256("fn b() { 2 }"), "-".to_string()];
    assert_eq!(stale_files(&files, &hashes), Some((vec!["/workspace/b.rs".to_string()], vec!["/workspace/c.rs".to_string()])));
    // a is gone and c changed: each answer goes with its own file.
    let hashes = vec!["-".to_string(), sha256("fn b() {}"), sha256("fn c() { 3 }")];
    assert_eq!(stale_files(&files, &hashes), Some((vec!["/workspace/c.rs".to_string()], vec!["/workspace/a.rs".to_string()])));
    // A file that isn't UTF-8 was synced as lossy text, which never hashes
    // like the bytes on disk: it's read again (and `sync` sends nothing,
    // the lossy text being the same).
    let lossy = open(&[("/workspace/bin.rs", &String::from_utf8_lossy(b"\xff\xfe"))]);
    let bytes = {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(b"\xff\xfe"))
    };
    assert_eq!(stale_files(&lossy, &[bytes]), Some((vec!["/workspace/bin.rs".to_string()], vec![])));
    // An answer that doesn't line up with the files is no answer.
    assert_eq!(stale_files(&files, &same[..2]), None);
    assert_eq!(stale_files(&files, &[same.clone(), vec!["-".to_string()]].concat()), None);
}

/// SME-172: a rename's edits are applied only to files the server saw as
/// they are now; a file changed since, or never sent, is listed.
#[test]
fn rename_check_lists_files_the_server_saw_otherwise() {
    let touched = |path: &str, current: &str, synced: Option<&str>| (path.to_string(), current.to_string(), synced.map(str::to_string));
    assert_eq!(rename_check(&[touched("/workspace/a.rs", "fn a() {}", Some("fn a() {}"))]), Vec::<String>::new());
    assert_eq!(
        rename_check(&[
            touched("/workspace/a.rs", "fn a() {}", Some("fn a() {}")),
            touched("/workspace/b.rs", "fn b() {}\n// more\n", Some("fn b() {}\n")),
            touched("/workspace/c.rs", "fn c() {}", None),
        ]),
        vec!["/workspace/b.rs".to_string(), "/workspace/c.rs".to_string()]
    );
}
