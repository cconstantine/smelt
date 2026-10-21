//! SME-181: a trust decision and a load racing for the same remote.
//! Each test forces one named interleaving with a row lock (the pattern
//! in tdd.md): the test's own uncommitted transaction holds a row the
//! first caller needs (an inserted row its insert conflicts with, or an
//! existing row held `FOR UPDATE` that its delete must wait for), so
//! that caller stops mid-way; the second caller then runs, and the test
//! rolls back once it has finished or is waiting too.

use super::db_tests::{cloned_repo, file, only_repo};
use super::*;
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(10);

/// One race test at a time. Each holds about four connections at once
/// (the held row, two callers, the poll), and every `#[sqlx::test]` in
/// the process shares sqlx's parent pool of 20: run together, they ran
/// out (`PoolTimedOut`). A tokio mutex, since each test has its own
/// runtime.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_trust_and_decline_on_one_card_settle_once(pool: PgPool) {
    let _turn = ONE_AT_A_TIME.lock().await;
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
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_trust_and_decline_on_two_conversations_cards_settle_once(pool: PgPool) {
    let _turn = ONE_AT_A_TIME.lock().await;
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
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_card_asked_for_while_a_decline_waits_is_dropped_by_it(pool: PgPool) {
    let _turn = ONE_AT_A_TIME.lock().await;
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

/// Holds every row of `table` for `repo_id` with `SELECT … FOR UPDATE`,
/// uncommitted, so whoever writes them next waits for the rollback.
async fn hold_rows<'a>(pool: &'a PgPool, table: &str, repo_id: i64) -> sqlx::Transaction<'a, sqlx::Postgres> {
    let mut holding = pool.begin().await.expect("begin the holding transaction");
    sqlx::query(&format!("SELECT 1 FROM {table} WHERE repo_id = $1 FOR UPDATE"))
        .bind(repo_id)
        .execute(&mut *holding)
        .await
        .expect("hold the rows");
    holding
}

/// Two conversations about the same remote, with a card in A. B is
/// the one that will be deleted.
async fn a_card_and_b(pool: &PgPool) -> (i64, TrustRequest, i64, db::ConversationRepo) {
    let a = db::create_conversation(pool).await.expect("conversation A");
    let b = db::create_conversation(pool).await.expect("conversation B");
    let repo_a = cloned_repo(pool, a.id).await;
    let repo_b = cloned_repo(pool, b.id).await;
    request_or_load(pool, a.id, &repo_a, "AGENTS.md", &file("a")).await.expect("ask in A");
    let card_a = only_repo(pool, a.id).await.trust_requests.remove(0);
    (a.id, card_a, b.id, repo_b)
}

fn spawn_delete(pool: &PgPool, conversation_id: i64) -> tokio::task::JoinHandle<Result<(), sqlx::Error>> {
    let pool = pool.clone();
    tokio::spawn(async move { db::delete_conversation(&pool, conversation_id).await })
}

/// SME-181 review 1, finding 1. Interleaving: deleting conversation B
/// (a file loaded and a card, both from the remote) is held at its
/// cascade into `loaded_instructions` while Decline answers A's card.
/// The delete's cascade takes B's loaded file and then its card; a
/// Decline that dropped B's card before unloading took them the other
/// way round, and Postgres aborted one of the two as a deadlock.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_decline_and_a_conversation_delete_dont_deadlock(pool: PgPool) {
    let _turn = ONE_AT_A_TIME.lock().await;
    let (a, card_a, b, repo_b) = a_card_and_b(&pool).await;
    db::load_instruction(&pool, b, repo_b.id, "AGENTS.md", &file("loaded")).await.expect("loaded in B");
    db::request_instruction(&pool, b, repo_b.id, "web/AGENTS.md", &file("asked")).await.expect("a card in B");

    let holding = hold_rows(&pool, "loaded_instructions", repo_b.id).await;
    let delete = spawn_delete(&pool, b);
    assert!(waiters_or_finished(&pool, 1, &delete).await, "the delete never reached B's loaded file");
    let decline = spawn_decide(&pool, a, &card_a, false);
    let decline_waited = waiters_or_finished(&pool, 2, &decline).await;
    holding.rollback().await.expect("release B's loaded file");

    let deleted = join(delete, "the delete").await;
    let declined = join(decline, "Decline").await;
    deleted.expect("the delete succeeds");
    declined.expect("Decline succeeds");
    assert_eq!(db::get_repo_trust(&pool, "github.com/o/r").await.expect("trust"), Some(false));
    assert!(decline_waited, "Decline waited for the delete rather than running first");
}

/// SME-181 review 1, finding 2. Interleaving: deleting conversation B
/// (a card about the remote) is held at its cascade into B's card while
/// Decline answers A's card and has listed B's card among the others.
/// A conversation deleted meanwhile is skipped, not a failed decision.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_conversation_deleted_while_a_decline_drops_its_card_doesnt_fail_it(pool: PgPool) {
    let _turn = ONE_AT_A_TIME.lock().await;
    let (a, card_a, b, repo_b) = a_card_and_b(&pool).await;
    db::request_instruction(&pool, b, repo_b.id, "AGENTS.md", &file("asked")).await.expect("a card in B");

    let holding = hold_rows(&pool, "instruction_requests", repo_b.id).await;
    let delete = spawn_delete(&pool, b);
    assert!(waiters_or_finished(&pool, 1, &delete).await, "the delete never reached B's card");
    let decline = spawn_decide(&pool, a, &card_a, false);
    let decline_waited = waiters_or_finished(&pool, 2, &decline).await;
    holding.rollback().await.expect("release B's card");

    join(delete, "the delete").await.expect("the delete succeeds");
    let notices = join(decline, "Decline").await.expect("Decline succeeds");
    assert_eq!(db::get_repo_trust(&pool, "github.com/o/r").await.expect("trust"), Some(false));
    assert!(notices.iter().all(|(id, _)| *id != b), "nothing is told to a deleted conversation: {notices:?}");
    assert!(decline_waited, "Decline waited for the delete rather than running first");
}

/// SME-181 review 1, finding 1, the deciding conversation's own delete.
/// Interleaving: Trust on A's card is held at its load while A is
/// deleted. Trust took its card before its load's foreign key took a
/// share lock on A's row, and the delete takes A's row before its card,
/// so the two deadlocked. Trust now locks A's row first.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_trust_and_a_delete_of_its_own_conversation_dont_deadlock(pool: PgPool) {
    let _turn = ONE_AT_A_TIME.lock().await;
    let (a, card_a, _, _) = a_card_and_b(&pool).await;
    let repo_a = only_repo(&pool, a).await;

    let holding = hold_row(&pool, "loaded_instructions", a, repo_a.id).await;
    let trust = spawn_decide(&pool, a, &card_a, true);
    assert!(waiters_or_finished(&pool, 1, &trust).await, "Trust never reached its load");
    let delete = spawn_delete(&pool, a);
    let delete_waited = waiters_or_finished(&pool, 2, &delete).await;
    holding.rollback().await.expect("release the load");

    let trusted = join(trust, "Trust").await;
    let deleted = join(delete, "the delete").await;
    trusted.expect("Trust succeeds");
    deleted.expect("the delete succeeds");
    assert!(delete_waited, "the delete waited for Trust rather than running first");
}

/// Physical order of `table`'s rows for `repo_id`, by id.
async fn physical_order(pool: &PgPool, table: &str, repo_id: i64) -> Vec<i64> {
    sqlx::query_scalar(&format!("SELECT id FROM {table} WHERE repo_id = $1 ORDER BY ctid"))
        .bind(repo_id)
        .fetch_all(pool)
        .await
        .expect("physical order")
}

/// Holds one row of `table` with `SELECT … FOR UPDATE`, uncommitted.
async fn hold_one<'a>(pool: &'a PgPool, table: &str, id: i64) -> sqlx::Transaction<'a, sqlx::Postgres> {
    let mut holding = pool.begin().await.expect("begin the holding transaction");
    sqlx::query(&format!("SELECT 1 FROM {table} WHERE id = $1 FOR UPDATE"))
        .bind(id)
        .execute(&mut *holding)
        .await
        .expect("hold the row");
    holding
}

/// Runs B's delete held at `held`, then Decline on A's card, releases
/// `held`, and checks both went through.
async fn decline_while_deleting(pool: &PgPool, a: i64, card_a: &TrustRequest, b: i64, holding: sqlx::Transaction<'_, sqlx::Postgres>) {
    let delete = spawn_delete(pool, b);
    assert!(waiters_or_finished(pool, 1, &delete).await, "the delete never reached the held row");
    let decline = spawn_decide(pool, a, card_a, false);
    let decline_waited = waiters_or_finished(pool, 2, &decline).await;
    holding.rollback().await.expect("release the held row");

    let deleted = join(delete, "the delete").await;
    let declined = join(decline, "Decline").await;
    deleted.expect("the delete succeeds");
    declined.expect("Decline succeeds");
    assert_eq!(db::get_repo_trust(pool, "github.com/o/r").await.expect("trust"), Some(false));
    assert!(decline_waited, "Decline waited for the delete rather than running first");
}

/// SME-181 review 2, finding 1, cards. Interleaving: B has two cards
/// about the remote, the first asked for again so its row moved past
/// the second's. B's delete cascades into them in that physical order
/// and is held at the second card while Decline answers A's card, which
/// drops B's cards in id order.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_decline_and_a_delete_of_a_conversation_with_two_cards_dont_deadlock(pool: PgPool) {
    let _turn = ONE_AT_A_TIME.lock().await;
    let (a, card_a, b, repo_b) = a_card_and_b(&pool).await;
    let first = db::request_instruction(&pool, b, repo_b.id, "AGENTS.md", &file("x1")).await.expect("first card");
    let second = db::request_instruction(&pool, b, repo_b.id, "web/AGENTS.md", &file("y")).await.expect("second card");
    db::request_instruction(&pool, b, repo_b.id, "AGENTS.md", &file("x2")).await.expect("the first, asked again");
    assert_eq!(
        physical_order(&pool, "instruction_requests", repo_b.id).await,
        vec![second.id, first.id],
        "the first card's row moved past the second's"
    );

    let holding = hold_one(&pool, "instruction_requests", second.id).await;
    decline_while_deleting(&pool, a, &card_a, b, holding).await;
}

/// SME-181 review 2, finding 1, loaded files. Interleaving: B has two
/// files loaded from the remote, the first loaded again so its row
/// moved. B's delete is held at the second while Decline answers A's
/// card and unloads both through the index.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_decline_and_a_delete_of_a_conversation_with_two_loaded_files_dont_deadlock(pool: PgPool) {
    let _turn = ONE_AT_A_TIME.lock().await;
    let (a, card_a, b, repo_b) = a_card_and_b(&pool).await;
    let first = db::load_instruction(&pool, b, repo_b.id, "AGENTS.md", &file("x1")).await.expect("first file");
    let second = db::load_instruction(&pool, b, repo_b.id, "web/AGENTS.md", &file("y")).await.expect("second file");
    db::load_instruction(&pool, b, repo_b.id, "AGENTS.md", &file("x2")).await.expect("the first, loaded again");
    assert_eq!(
        physical_order(&pool, "loaded_instructions", repo_b.id).await,
        vec![second.id, first.id],
        "the first file's row moved past the second's"
    );

    let holding = hold_one(&pool, "loaded_instructions", second.id).await;
    decline_while_deleting(&pool, a, &card_a, b, holding).await;
}

fn spawn_attach(pool: &PgPool, conversation_id: i64) -> tokio::task::JoinHandle<Result<Vec<(i64, String)>, String>> {
    let pool = pool.clone();
    tokio::spawn(async move {
        start_attach(&pool, conversation_id, "https://github.com/o/r", None, None)
            .await
            .map(|(_, _, notices)| notices)
    })
}

/// SME-224. Interleaving: conversation B's `request_or_load` (the remote
/// undecided) is held at its card's insert while conversation A starts
/// "Work on a repo" on the same remote. The attach waits for the load,
/// then drops B's card and tells B, rather than miss a card committed
/// after it listed the remote's cards.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_card_asked_for_while_work_on_a_repo_waits_is_dropped_by_it(pool: PgPool) {
    let _turn = ONE_AT_A_TIME.lock().await;
    let a = db::create_conversation(&pool).await.expect("conversation A");
    let b = db::create_conversation(&pool).await.expect("conversation B");
    let repo_b = cloned_repo(&pool, b.id).await;

    let holding = hold_row(&pool, "instruction_requests", b.id, repo_b.id).await;
    let load = tokio::spawn({
        let (pool, repo_b) = (pool.clone(), repo_b.clone());
        async move { request_or_load(&pool, b.id, &repo_b, "AGENTS.md", &file("b")).await }
    });
    assert!(waiters_or_finished(&pool, 1, &load).await, "the load never reached its card");
    let attach = spawn_attach(&pool, a.id);
    let attach_waited = waiters_or_finished(&pool, 2, &attach).await;
    holding.rollback().await.expect("release the card");

    assert_eq!(join(load, "the load").await.expect("the load"), LoadOutcome::AwaitingTrust);
    let notices = join(attach, "the attach").await.expect("the attach");
    assert_eq!(db::get_repo_trust(&pool, "github.com/o/r").await.expect("trust"), Some(true));
    assert!(only_repo(&pool, b.id).await.trust_requests.is_empty(), "B's card is dropped");
    assert!(
        notices.iter().any(|(id, text)| *id == b.id && text.contains("load_instructions again")),
        "B's model is told: {notices:?}"
    );
    assert!(attach_waited, "the attach waited for the load rather than running first");
}

/// SME-224. Interleaving: deleting conversation B (a card about the
/// remote) is held at its cascade into B's card while A starts "Work on
/// a repo" on the same remote. A conversation deleted meanwhile is
/// skipped, not a failed attach ("No such repo.").
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_conversation_deleted_while_work_on_a_repo_drops_its_card_doesnt_fail_it(pool: PgPool) {
    let _turn = ONE_AT_A_TIME.lock().await;
    let (a, _, b, repo_b) = a_card_and_b(&pool).await;
    db::request_instruction(&pool, b, repo_b.id, "AGENTS.md", &file("asked")).await.expect("a card in B");

    let holding = hold_rows(&pool, "instruction_requests", repo_b.id).await;
    let delete = spawn_delete(&pool, b);
    assert!(waiters_or_finished(&pool, 1, &delete).await, "the delete never reached B's card");
    let attach = spawn_attach(&pool, a);
    let attach_waited = waiters_or_finished(&pool, 2, &attach).await;
    holding.rollback().await.expect("release B's card");

    let deleted = join(delete, "the delete").await;
    let attached = join(attach, "the attach").await;
    deleted.expect("the delete succeeds");
    let notices = attached.expect("the attach succeeds");
    assert_eq!(db::get_repo_trust(&pool, "github.com/o/r").await.expect("trust"), Some(true));
    assert!(notices.iter().all(|(id, _)| *id != b), "nothing is told to a deleted conversation: {notices:?}");
    assert!(attach_waited, "the attach waited for the delete rather than running first");
}

/// SME-224, the SME-181 review-2 shape. Interleaving: B has two cards
/// about the remote, the first asked for again so its row moved past
/// the second's. B's delete cascades into them in that physical order
/// and is held at the second card while A starts "Work on a repo",
/// which drops the remote's cards in id order. Without the conversation
/// share lock first, the two would take the cards in opposite orders.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_work_on_a_repo_and_a_delete_of_a_conversation_with_two_cards_dont_deadlock(pool: PgPool) {
    let _turn = ONE_AT_A_TIME.lock().await;
    let (a, _, b, repo_b) = a_card_and_b(&pool).await;
    let first = db::request_instruction(&pool, b, repo_b.id, "AGENTS.md", &file("x1")).await.expect("first card");
    let second = db::request_instruction(&pool, b, repo_b.id, "web/AGENTS.md", &file("y")).await.expect("second card");
    db::request_instruction(&pool, b, repo_b.id, "AGENTS.md", &file("x2")).await.expect("the first, asked again");
    assert_eq!(
        physical_order(&pool, "instruction_requests", repo_b.id).await,
        vec![second.id, first.id],
        "the first card's row moved past the second's"
    );

    let holding = hold_one(&pool, "instruction_requests", second.id).await;
    let delete = spawn_delete(&pool, b);
    assert!(waiters_or_finished(&pool, 1, &delete).await, "the delete never reached the held card");
    let attach = spawn_attach(&pool, a);
    let attach_waited = waiters_or_finished(&pool, 2, &attach).await;
    holding.rollback().await.expect("release the held card");

    let deleted = join(delete, "the delete").await;
    let attached = join(attach, "the attach").await;
    deleted.expect("the delete succeeds");
    attached.expect("the attach succeeds");
    assert_eq!(db::get_repo_trust(&pool, "github.com/o/r").await.expect("trust"), Some(true));
    assert!(attach_waited, "the attach waited for the delete rather than running first");
}
