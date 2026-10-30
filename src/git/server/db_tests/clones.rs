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

/// SME-283 (review 1, F1): the refusal comes before the trust on the
/// user's path too. `start_attach` trusts the named remote right after
/// planning — a url that hides characters must be refused inside the
/// plan, so a remote the user can't fully see is never granted trust,
/// then recorded. Green-first pin: it fails if planning and trust ever
/// swap order or the refusal skips this path.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_work_on_a_repo_refuses_a_dirty_url_before_trusting_it(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let failed = start_attach(&pool, conversation.id, "file:///tmp/o\u{200b}rigin.git", None, None)
        .await
        .expect_err("a url hiding a character is refused");
    assert_eq!(failed.notices, Vec::new(), "nothing to notice: nothing was dropped");
    assert!(failed.error.contains("\\u{200b}"), "the refusal is quoted escaped: {}", failed.error);
    assert!(list_repos(&pool, conversation.id).await.expect("list").is_empty(), "no repo row");
    let trust_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_trust")
        .fetch_one(&pool)
        .await
        .expect("trust count");
    assert_eq!(trust_rows, 0, "no trust before the refusal");
}

/// SME-283 (review 1, F2): the dir-clash refusal is a fifth showing of a
/// recorded remote — the row in the way is named, and a row recorded
/// before the fix can hold one whose difference from another is invisible.
/// It quotes through the same escape; the raw codepoint never rides.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_dir_clash_refusal_quotes_the_other_row_escaped(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    // A legacy row: written straight to the db, as rows recorded before
    // the refusal were — its url hides a character.
    db::create_conversation_repo(&pool, conversation.id, "git\u{200b}hub.com/o/r", "git\u{200b}hub.com/o/r", None, "r")
        .await
        .expect("legacy row");
    let err = clone_repo(&pool, conversation.id, "git@github.com:o/other.git", None, Some("r"))
        .await
        .expect_err("the directory is in use");
    assert!(err.contains("already used by"), "{err}");
    assert!(err.contains("\\u{200b}"), "the clash quotes the other remote escaped: {err}");
    assert!(!err.contains('\u{200b}'), "the raw character is never quoted: {err:?}");
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
