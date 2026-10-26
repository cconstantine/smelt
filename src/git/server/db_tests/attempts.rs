//! Clone attempts: guards, retries, superseded attempts and lost races.

use super::*;

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_clone_dropped_midway_is_marked_interrupted(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let repo = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
        .await
        .expect("repo");
    // Finished: left alone.
    CloneGuard::new(&pool, &repo, conversation.id).finish();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let row = db::get_conversation_repo(&pool, repo.id).await.expect("get").expect("exists");
    assert_eq!(row.status, "cloning");
    // Dropped mid-clone: marked failed, so turns stop waiting on it.
    drop(CloneGuard::new(&pool, &repo, conversation.id));
    let marked = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let row = db::get_conversation_repo(&pool, repo.id).await.expect("get").expect("exists");
            if row.status == "failed" {
                return row;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the dropped clone is marked failed");
    assert_eq!(marked.error.as_deref(), Some(CLONE_INTERRUPTED));
    assert!(wait_for_clones(&pool, conversation.id, std::time::Duration::from_millis(100)).await);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_after_the_clone_is_recorded_doesnt_mark_it_interrupted(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let repo = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
        .await
        .expect("repo");
    let guard = CloneGuard::new(&pool, &repo, conversation.id);
    let cloned = ClonedRepo {
        commit: Some("abc".to_string()),
        branch: "main".to_string(),
    };
    record_cloned(&pool, &repo, &cloned, Ok(no_agents()), Ok(vec![]), guard).await.expect("record");
    // What follows is cut off here.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let row = db::get_conversation_repo(&pool, repo.id).await.expect("get").expect("exists");
    assert_eq!((row.status.as_str(), row.error.as_deref()), ("ready", None));
}

/// A turn stopped just after the clone was recorded ready, before
/// its guard was disarmed: the guard's "interrupted" doesn't undo
/// it (SME-86).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_guard_firing_after_ready_leaves_the_clone_ready(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let repo = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
        .await
        .expect("repo");
    let guard = CloneGuard::new(&pool, &repo, conversation.id);
    db::set_repo_ready(&pool, repo.id, repo.attempt, "main", Some("abc"), &[], &[], None)
        .await
        .expect("ready");
    drop(guard);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let row = db::get_conversation_repo(&pool, repo.id).await.expect("get").expect("exists");
    assert_eq!((row.status.as_str(), row.error.as_deref()), ("ready", None));
}

/// A replaced attempt whose clone (or pod start) fails is told it
/// was superseded, not that the repo failed (SME-86 code review).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_superseded_attempt_that_fails_is_told_so(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let stale = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
        .await
        .expect("repo");
    assert_eq!(fail_attempt(&pool, &stale, "fatal: nope").await, "fatal: nope");
    db::retry_repo_clone(&pool, stale.id, "u", "k", None)
        .await
        .expect("retry")
        .expect("it was failed");
    assert_eq!(fail_attempt(&pool, &stale, "fatal: again").await, CLONE_SUPERSEDED);
    let row = db::get_conversation_repo(&pool, stale.id).await.expect("get").expect("exists");
    assert_eq!(row.status, "cloning");
}

/// A retry that loses the race to another retry of the same failed
/// clone is refused, saying why (SME-86).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_retry_racing_another_is_refused(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let repo = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
        .await
        .expect("repo");
    db::set_repo_failed(&pool, repo.id, repo.attempt, "fatal: nope").await.expect("fail");
    assert!(record_clone(&pool, conversation.id, "u", None, "k", "r", Some(repo.id))
        .await
        .expect("the first retry")
        .is_some());
    assert!(
        record_clone(&pool, conversation.id, "u", None, "k", "r", Some(repo.id))
            .await
            .expect("the second retry")
            .is_none(),
        "the second retry runs nothing"
    );
}

/// Two fresh clones into the same directory at once (\"Work on a
/// repo\" while the model clones it): the second gets the first's
/// checkout, not a database error (SME-86 code review 2).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_fresh_clone_that_lost_the_race_gets_the_winners_checkout(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let url = "git@github.com:o/r.git";
    let first = record_clone(&pool, conversation.id, url, None, "github.com/o/r", "r", None)
        .await
        .expect("the first clone")
        .expect("recorded");
    let second = record_clone(&pool, conversation.id, url, None, "github.com/o/r", "r", None)
        .await
        .expect("the second clone isn't a database error");
    assert!(second.is_none(), "the second runs nothing: {second:?}");
    let existing = after_lost_race(&pool, conversation.id, url, None, "r").await.expect("its checkout");
    assert_eq!(existing.id, first.id);
}

/// A request whose retry lost the race gets the other request's
/// checkout when it's the same repo and branch, cloning or done, and
/// the usual refusal otherwise (SME-86 code review).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_retry_that_lost_the_race_gets_the_winners_checkout(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let url = "git@github.com:o/r.git";
    let repo = db::create_conversation_repo(&pool, conversation.id, url, "github.com/o/r", None, "r")
        .await
        .expect("repo");
    // Still cloning for the winner: this request waits on the same row.
    let cloning = after_lost_race(&pool, conversation.id, url, None, "r").await.expect("cloning");
    assert_eq!((cloning.id, cloning.status.as_str()), (repo.id, "cloning"));
    // The winner finished: the checkout is this request's too.
    db::set_repo_ready(&pool, repo.id, repo.attempt, "main", Some("abc"), &[], &[], None)
        .await
        .expect("ready");
    let ready = after_lost_race(&pool, conversation.id, url, None, "r").await.expect("ready");
    assert_eq!((ready.id, ready.status.as_str()), (repo.id, "ready"));
    // Another repo is there now: refused, as a fresh request would be.
    let other = after_lost_race(&pool, conversation.id, "git@github.com:o/other.git", None, "r")
        .await
        .expect_err("another repo");
    assert!(other.contains("already used by"), "{other}");
}

/// An attempt that a retry has replaced can't record its clone as
/// the repo's, and says so (SME-86).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_superseded_attempt_doesnt_record_its_clone(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let stale = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
        .await
        .expect("repo");
    db::set_repo_failed(&pool, stale.id, stale.attempt, "fatal: nope").await.expect("fail");
    let current = db::retry_repo_clone(&pool, stale.id, "u", "k", None)
        .await
        .expect("retry")
        .expect("it was failed");
    let cloned = ClonedRepo { commit: Some("abc".to_string()), branch: "main".to_string() };
    let guard = CloneGuard::new(&pool, &stale, conversation.id);
    let refused = record_cloned(&pool, &stale, &cloned, Ok(no_agents()), Ok(vec![]), guard)
        .await
        .expect_err("superseded");
    assert!(refused.contains("superseded"), "{refused}");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let row = db::get_conversation_repo(&pool, stale.id).await.expect("get").expect("exists");
    assert_eq!((row.status.as_str(), row.attempt), ("cloning", current.attempt));
}

/// A repo is ready only once its AGENTS.md files are listed, so a
/// turn that waited for the clone sees them (SME-32 code review 10,
/// finding 3).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_repo_is_ready_with_its_agents_files_already_listed(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let cloned = ClonedRepo { commit: Some("abc".to_string()), branch: "main".to_string() };

    let listed = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "listed").await.expect("repo");
    let guard = CloneGuard::new(&pool, &listed, conversation.id);
    let files = vec!["AGENTS.md".to_string(), "api/AGENTS.md".to_string()];
    record_cloned(&pool, &listed, &cloned, Ok(AgentsPaths { files: files.clone(), left_out: 0 }), Ok(vec![]), guard).await.expect("record");
    let row = db::get_conversation_repo(&pool, listed.id).await.expect("get").expect("exists");
    assert_eq!((row.status.as_str(), row.error.as_deref(), row.agents_files), ("ready", None, files));

    let unlisted = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "unlisted").await.expect("repo");
    let guard = CloneGuard::new(&pool, &unlisted, conversation.id);
    record_cloned(&pool, &unlisted, &cloned, Err("exec failed".to_string()), Ok(vec![]), guard).await.expect("record");
    let row = db::get_conversation_repo(&pool, unlisted.id).await.expect("get").expect("exists");
    assert_eq!(row.status, "ready", "the clone itself is good");
    assert_eq!(row.error.as_deref(), Some("Couldn't list its AGENTS.md files: exec failed"));
}
