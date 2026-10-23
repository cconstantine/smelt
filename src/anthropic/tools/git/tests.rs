use super::*;

fn ready_repo(branch: &str) -> crate::git::RepoSummary {
    crate::git::RepoSummary {
        id: 1,
        url: "git@github.com:o/r.git".to_string(),
        path: "/workspace/r".to_string(),
        requested_branch: None,
        branch: Some(branch.to_string()),
        commit: Some("abc".to_string()),
        status: crate::git::RepoStatus::Ready,
        error: None,
        agents_files: vec![],
        loaded_instructions: vec![],
        trust_requests: vec![],
    }
}

/// SME-272: serde_json leaves a line separator or bidi control raw, so a
/// checked-out branch (the remote owner's choice) that isn't plain text
/// is left out of clone_repo's result, and a note says why.
#[test]
fn test_clone_result_doesnt_show_a_branch_name_that_breaks_a_line() {
    let told = clone_result_for_model(ready_repo("main\u{2028}x"), &[]);
    assert!(!told.contains('\u{2028}'), "{told}");
    let parsed: Value = serde_json::from_str(&told).expect("JSON");
    assert_eq!(parsed["branch"], Value::Null, "{told}");
    assert!(parsed["note"].as_str().is_some_and(|n| n.contains("control characters")), "{told}");

    let told = clone_result_for_model(ready_repo("main"), &[]);
    let parsed: Value = serde_json::from_str(&told).expect("JSON");
    assert_eq!(parsed["branch"], "main", "{told}");
    assert!(!told.contains("control characters"), "{told}");
}
