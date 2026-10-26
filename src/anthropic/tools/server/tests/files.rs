//! The file tools: prior-hash bookkeeping and edit_file's refusals.

use super::*;

#[test]
fn test_find_prior_file_hash_returns_none_when_path_never_touched() {
    let messages = vec![message_with_blocks(
        1,
        vec![
            tool_use("t1", "read_file", serde_json::json!({"path": "/a.txt"})),
            tool_result(
                "t1",
                serde_json::json!({"content": "x", "hash": "hash-a"}),
                false,
            ),
        ],
    )];
    assert_eq!(find_prior_file_hash(&messages, "/other.txt"), None);
}

#[test]
fn test_find_prior_file_hash_returns_hash_from_a_prior_read_file() {
    let messages = vec![message_with_blocks(
        1,
        vec![
            tool_use("t1", "read_file", serde_json::json!({"path": "/a.txt"})),
            tool_result(
                "t1",
                serde_json::json!({"content": "x", "hash": "hash-a"}),
                false,
            ),
        ],
    )];
    assert_eq!(
        find_prior_file_hash(&messages, "/a.txt"),
        Some("hash-a".to_string())
    );
}

#[test]
fn test_find_prior_file_hash_returns_the_most_recent_hash_across_multiple_operations() {
    let messages = vec![
        message_with_blocks(
            1,
            vec![
                tool_use("t1", "read_file", serde_json::json!({"path": "/a.txt"})),
                tool_result(
                    "t1",
                    serde_json::json!({"content": "x", "hash": "hash-1"}),
                    false,
                ),
            ],
        ),
        message_with_blocks(
            2,
            vec![
                tool_use(
                    "t2",
                    "edit_file",
                    serde_json::json!({"path": "/a.txt", "old_string": "x", "new_string": "y"}),
                ),
                tool_result("t2", serde_json::json!({"hash": "hash-2"}), false),
            ],
        ),
    ];
    assert_eq!(
        find_prior_file_hash(&messages, "/a.txt"),
        Some("hash-2".to_string())
    );
}

#[test]
fn test_find_prior_file_hash_ignores_an_errored_result() {
    let messages = vec![message_with_blocks(
        1,
        vec![
            tool_use(
                "t1",
                "edit_file",
                serde_json::json!({"path": "/a.txt", "old_string": "x", "new_string": "y"}),
            ),
            tool_result("t1", serde_json::json!({"error": "ambiguous match"}), true),
        ],
    )];
    assert_eq!(find_prior_file_hash(&messages, "/a.txt"), None);
}

#[tokio::test]
async fn test_edit_file_tool_rejects_replace_all_and_expected_line_together() {
    let pool = test_pool();
    let result = edit_file_tool(
        &pool,
        1,
        &serde_json::json!({
            "path": "/a.txt", "old_string": "x", "new_string": "y",
            "replace_all": true, "expected_line": 3
        }),
    )
    .await;
    let message = result.expect_err("expected replace_all + expected_line to be rejected");
    assert!(
        message.contains("mutually exclusive"),
        "expected a mutual-exclusivity error, got: {message}"
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_edit_file_tool_refuses_when_path_never_read(pool: sqlx::PgPool) {
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let result = edit_file_tool(
        &pool,
        conversation.id,
        &serde_json::json!({"path": "/never-read.txt", "old_string": "x", "new_string": "y"}),
    )
    .await;
    let message = result.expect_err("expected a refusal when the path was never read");
    assert!(
        message.contains("read_file"),
        "expected an error telling the model to call read_file first, got: {message}"
    );
}

