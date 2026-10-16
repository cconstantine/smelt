//! SME-181: a trust decision and a load racing for the same remote.
//! Each test forces one named interleaving with a row lock (the pattern
//! in tdd.md): the test's own uncommitted transaction holds the row the
//! first caller's insert conflicts with, so that caller stops mid-way;
//! the second caller then runs, and the test rolls back once it has
//! finished or is waiting too.

use super::db_tests::{cloned_repo, file, only_repo};
use super::*;
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(10);

/// Sessions in this test's database waiting on a lock (a row lock or
/// the remote's advisory lock).
async fn lock_waiters(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity
          WHERE datname = current_database() AND wait_event_type = 'Lock'",
    )
    .fetch_one(pool)
    .await
    .expect("pg_stat_activity")
}

/// Waits until `n` sessions wait on a lock, or `task` has finished.
/// True when the waiters were seen.
async fn waiters_or_finished<T>(pool: &PgPool, n: i64, task: &tokio::task::JoinHandle<T>) -> bool {
    let deadline = Instant::now() + LIMIT;
    loop {
        if lock_waiters(pool).await >= n {
            return true;
        }
        if task.is_finished() {
            return false;
        }
        assert!(Instant::now() < deadline, "neither {n} lock waiters nor the task finishing");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn join<T>(task: tokio::task::JoinHandle<T>, what: &str) -> T {
    tokio::time::timeout(LIMIT, task)
        .await
        .unwrap_or_else(|_| panic!("{what} didn't finish"))
        .unwrap_or_else(|e| panic!("{what} task: {e}"))
}

/// Holds `(repo_id, path)` in `table` with an uncommitted insert, so a
/// caller's own insert of that row waits until the test rolls back.
async fn hold_row<'a>(pool: &'a PgPool, table: &str, conversation_id: i64, repo_id: i64) -> sqlx::Transaction<'a, sqlx::Postgres> {
    let mut holding = pool.begin().await.expect("begin the holding transaction");
    sqlx::query(&format!(
        "INSERT INTO {table} (conversation_id, repo_id, path, content, file_bytes, hash)
         VALUES ($1, $2, 'AGENTS.md', 'held', 4, 'held')"
    ))
    .bind(conversation_id)
    .bind(repo_id)
    .execute(&mut *holding)
    .await
    .expect("hold the row, uncommitted");
    holding
}

fn spawn_decide(
    pool: &PgPool,
    conversation_id: i64,
    request: &TrustRequest,
    trusted: bool,
) -> tokio::task::JoinHandle<Result<Vec<(i64, String)>, String>> {
    let (pool, id, hash) = (pool.clone(), request.id, request.hash.clone());
    tokio::spawn(async move { decide_trust(&pool, conversation_id, id, &hash, trusted).await })
}

/// B32. Interleaving: Trust on a card is held at its load while Decline
/// answers the same card (another tab). The card is answered once:
/// Trust stands and Decline is refused.
#[sqlx::test]
async fn test_trust_and_decline_on_one_card_settle_once(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let repo = cloned_repo(&pool, conversation.id).await;
    request_or_load(&pool, conversation.id, &repo, "AGENTS.md", &file("x")).await.expect("ask");
    let card = only_repo(&pool, conversation.id).await.trust_requests.remove(0);

    let holding = hold_row(&pool, "loaded_instructions", conversation.id, repo.id).await;
    let trust = spawn_decide(&pool, conversation.id, &card, true);
    assert!(waiters_or_finished(&pool, 1, &trust).await, "Trust never reached its load");
    let decline = spawn_decide(&pool, conversation.id, &card, false);
    let decline_waited = waiters_or_finished(&pool, 2, &decline).await;
    holding.rollback().await.expect("release the load");

    join(trust, "Trust").await.expect("Trust stands");
    let refused = join(decline, "Decline").await.expect_err("the second answer is refused");
    assert!(refused.contains("answered already"), "{refused}");
    assert_eq!(db::get_repo_trust(&pool, "github.com/o/r").await.expect("trust"), Some(true));
    let loaded = project_instructions(&pool, conversation.id).await.expect("loaded");
    assert_eq!(loaded.len(), 1, "the trusted file is loaded");
    assert_eq!(loaded[0].content, "x");
    assert!(only_repo(&pool, conversation.id).await.trust_requests.is_empty(), "no card left");
    assert!(decline_waited, "Decline waited for Trust rather than running first");
}

/// Interleaving: Trust on conversation A's card is held at its load
/// while Decline answers conversation B's card for the same remote.
/// Trust settles the remote, and B's card went with it.
#[sqlx::test]
async fn test_trust_and_decline_on_two_conversations_cards_settle_once(pool: PgPool) {
    let a = db::create_conversation(&pool).await.expect("conversation A");
    let b = db::create_conversation(&pool).await.expect("conversation B");
    let repo_a = cloned_repo(&pool, a.id).await;
    let repo_b = cloned_repo(&pool, b.id).await;
    request_or_load(&pool, a.id, &repo_a, "AGENTS.md", &file("a")).await.expect("ask in A");
    request_or_load(&pool, b.id, &repo_b, "AGENTS.md", &file("b")).await.expect("ask in B");
    let card_a = only_repo(&pool, a.id).await.trust_requests.remove(0);
    let card_b = only_repo(&pool, b.id).await.trust_requests.remove(0);

    let holding = hold_row(&pool, "loaded_instructions", a.id, repo_a.id).await;
    let trust = spawn_decide(&pool, a.id, &card_a, true);
    assert!(waiters_or_finished(&pool, 1, &trust).await, "Trust never reached its load");
    let decline = spawn_decide(&pool, b.id, &card_b, false);
    let decline_waited = waiters_or_finished(&pool, 2, &decline).await;
    holding.rollback().await.expect("release the load");

    let notices = join(trust, "Trust").await.expect("Trust stands");
    let refused = join(decline, "Decline").await.expect_err("the second answer is refused");
    assert!(refused.contains("answered already"), "{refused}");
    assert_eq!(db::get_repo_trust(&pool, "github.com/o/r").await.expect("trust"), Some(true));
    assert_eq!(project_instructions(&pool, a.id).await.expect("loaded in A").len(), 1);
    assert!(
        notices.iter().any(|(id, text)| *id == b.id && text.contains("now trusts")),
        "B's model is told: {notices:?}"
    );
    assert!(only_repo(&pool, a.id).await.trust_requests.is_empty(), "no card left in A");
    assert!(only_repo(&pool, b.id).await.trust_requests.is_empty(), "no card left in B");
    assert!(decline_waited, "Decline waited for Trust rather than running first");
}

/// The reader. Interleaving: conversation B's `request_or_load` (the
/// remote undecided) is held at its card's insert while Decline answers
/// conversation A's card for the same remote. The Decline drops B's
/// card and tells B, rather than leave it asking about a declined remote.
#[sqlx::test]
async fn test_a_card_asked_for_while_a_decline_waits_is_dropped_by_it(pool: PgPool) {
    let a = db::create_conversation(&pool).await.expect("conversation A");
    let b = db::create_conversation(&pool).await.expect("conversation B");
    let repo_a = cloned_repo(&pool, a.id).await;
    let repo_b = cloned_repo(&pool, b.id).await;
    request_or_load(&pool, a.id, &repo_a, "AGENTS.md", &file("a")).await.expect("ask in A");
    let card_a = only_repo(&pool, a.id).await.trust_requests.remove(0);

    let holding = hold_row(&pool, "instruction_requests", b.id, repo_b.id).await;
    let load = tokio::spawn({
        let (pool, repo_b) = (pool.clone(), repo_b.clone());
        async move { request_or_load(&pool, b.id, &repo_b, "AGENTS.md", &file("b")).await }
    });
    assert!(waiters_or_finished(&pool, 1, &load).await, "the load never reached its card");
    let decline = spawn_decide(&pool, a.id, &card_a, false);
    let decline_waited = waiters_or_finished(&pool, 2, &decline).await;
    holding.rollback().await.expect("release the card");

    assert_eq!(join(load, "the load").await.expect("the load"), LoadOutcome::AwaitingTrust);
    let notices = join(decline, "Decline").await.expect("Decline");
    assert_eq!(db::get_repo_trust(&pool, "github.com/o/r").await.expect("trust"), Some(false));
    assert!(only_repo(&pool, b.id).await.trust_requests.is_empty(), "B's card is dropped");
    assert!(
        notices.iter().any(|(id, text)| *id == b.id && text.contains("chose not to trust")),
        "B's model is told: {notices:?}"
    );
    assert!(decline_waited, "Decline waited for the load rather than running first");
}
