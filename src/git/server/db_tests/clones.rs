//! Cloning a repo for a conversation: refusals, checkout-dir names, waiting.

use super::*;

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_clone_repo_refuses_before_recording_anything(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let bad_url = clone_repo(&pool, conversation.id, "not a url", None, None)
        .await
        .expect_err("bad url");
    assert!(bad_url.contains("isn't a git URL"), "{bad_url}");
    let bad_dir = clone_repo(&pool, conversation.id, "git@github.com:o/r.git", None, Some("../etc"))
        .await
        .expect_err("bad dir");
    assert!(bad_dir.contains("directory"), "{bad_dir}");
    assert!(list_repos(&pool, conversation.id).await.expect("list").is_empty());
}

/// SME-283: a url that hides characters is refused on the path that
/// records — before any write, so it leaves no repo row and no trust —
/// and the refusal quotes the url escaped, so what the user reads names
/// the invisible characters instead of carrying them on.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_clone_refusal_quotes_the_url_escaped_and_records_nothing(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let err = clone_repo(&pool, conversation.id, "file:///tmp/o\u{200b}rigin.git", None, None)
        .await
        .expect_err("a url hiding a character is refused");
    assert!(err.contains("isn't a git URL"), "{err}");
    assert!(err.contains("\\u{200b}"), "the escape is shown: {err}");
    assert!(err.contains("render as nothing"), "the message says why: {err}");
    assert!(!err.contains('\u{200b}'), "the raw character is never quoted: {err:?}");
    assert!(list_repos(&pool, conversation.id).await.expect("list").is_empty(), "no repo row");
    let trust_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_trust")
        .fetch_one(&pool)
        .await
        .expect("trust count");
    assert_eq!(trust_rows, 0, "no trust either");
}

#[test]
fn test_checkout_dirs_are_one_plain_name() {
    // A repo like `org/.github` gets its own name by default.
    let github = default_checkout_dir("git@github.com:org/.github.git").expect("a dir");
    for ok in ["smelt", "my.repo", "repo-2", "a_b", ".github", github.as_str()] {
        assert!(validate_checkout_dir(ok).is_ok(), "{ok}");
    }
    for bad in ["", ".", "..", "../x", "a/b", "-rf", "has space", &"x".repeat(101)] {
        assert!(validate_checkout_dir(bad).is_err(), "{bad:?}");
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_wait_for_clones_waits_for_a_clone_to_finish(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    // Nothing cloning: no wait.
    assert!(wait_for_clones(&pool, conversation.id, std::time::Duration::from_secs(5)).await);
    let repo = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
        .await
        .expect("repo");
    // Still cloning at the deadline.
    assert!(!wait_for_clones(&pool, conversation.id, std::time::Duration::from_millis(300)).await);
    let finisher = {
        let pool = pool.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            db::set_repo_cloned(&pool, repo.id, "main", Some("abc")).await.expect("cloned");
        })
    };
    let started = std::time::Instant::now();
    assert!(wait_for_clones(&pool, conversation.id, std::time::Duration::from_secs(10)).await);
    assert!(started.elapsed() >= std::time::Duration::from_millis(250), "it waited");
    finisher.await.expect("finisher");
}
