use super::*;

/// SME-115: an empty database (a scratch, test or fresh one) migrates
/// to an instance of its own that owns no unlabelled cluster object.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_empty_database_gets_an_instance_that_owns_nothing_unlabelled(pool: PgPool) {
    let instance = smelt_instance(&pool).await.expect("the instance row");
    assert_eq!(instance.id.len(), 36, "a UUID: {:?}", instance.id);
    assert!(!instance.owns_unlabelled, "an empty database must never adopt another's objects");
    assert_eq!(smelt_instance(&pool).await.expect("read again"), instance, "fixed once made");
}

/// SME-115: a database that already had conversations when it
/// migrated (the dev database) owns the objects made before the fix.
#[sqlx::test(migrator = "crate::test_lock::NO_MIGRATIONS")]
async fn test_a_database_with_conversations_before_the_migration_owns_unlabelled_objects(pool: PgPool) {
    let all = sqlx::migrate!();
    let before = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            all.iter().filter(|m| m.version < SMELT_INSTANCE_MIGRATION).cloned().collect(),
        ),
        ..sqlx::migrate!()
    };
    assert!(before.iter().count() < all.iter().count(), "the instance migration is in the set");
    before.run(&pool).await.expect("migrate up to the instance migration");
    // A plain row: `create_conversation` reads back today's columns, which
    // the schema doesn't have yet.
    sqlx::query("INSERT INTO conversations (title) VALUES ('before the fix')")
        .execute(&pool)
        .await
        .expect("a conversation from before the fix");
    all.run(&pool).await.expect("the rest of the migrations");

    let instance = smelt_instance(&pool).await.expect("the instance row");
    assert!(instance.owns_unlabelled);
    assert_eq!(instance.id.len(), 36, "a UUID: {:?}", instance.id);
}

/// SME-115: a fresh database (a scratch check server's) starts its
/// ids far above the dev database's, so its pods and claims are never
/// named like the dev server's; once started, it isn't moved again.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_fresh_database_starts_its_ids_clear(pool: PgPool) {
    let base = start_ids_clear_if_fresh(&pool).await.expect("move them").expect("a fresh database is moved");
    assert!(base >= 2_000_000_000, "base {base}");
    let conversation = create_conversation(&pool).await.expect("conversation");
    let pod = create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    let volume = create_sandbox_volume(&pool, "sme-115", "/data").await.expect("volume row");
    for (what, id) in [("conversation", conversation.id), ("sandbox pod", pod.id), ("sandbox volume", volume.id)] {
        assert!(id > base, "the next {what} id is {id}, not above {base}");
    }
    assert_eq!(start_ids_clear_if_fresh(&pool).await.expect("again"), None, "moved twice");
}

/// SME-115 review 2: a move that failed partway (one sequence moved,
/// the others not) is finished at the next start, instead of leaving
/// the rest counting from 1 for good.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_partly_moved_database_finishes_the_move(pool: PgPool) {
    sqlx::query("SELECT setval(pg_get_serial_sequence('conversations', 'id'), 2000000005)")
        .execute(&pool)
        .await
        .expect("move one sequence");
    let base = start_ids_clear_if_fresh(&pool).await.expect("move them").expect("the rest are moved");
    let conversation = create_conversation(&pool).await.expect("conversation");
    let pod = create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    let volume = create_sandbox_volume(&pool, "sme-115", "/data").await.expect("volume row");
    assert_eq!(conversation.id, 2_000_000_006, "the moved sequence is kept");
    for (what, id) in [("sandbox pod", pod.id), ("sandbox volume", volume.id)] {
        assert!(id > base && base >= 2_000_000_000, "the next {what} id is {id}, base {base}");
    }
}

/// SME-115: a database whose ids were ever used (the dev or production
/// database) is left as it is.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_used_database_keeps_its_ids(pool: PgPool) {
    let first = create_conversation(&pool).await.expect("conversation");
    assert_eq!(start_ids_clear_if_fresh(&pool).await.expect("check"), None);
    let next = create_conversation(&pool).await.expect("conversation");
    assert_eq!(next.id, first.id + 1);
}

/// One instance per database: the table takes no second row.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_instance_table_holds_one_row(pool: PgPool) {
    smelt_instance(&pool).await.expect("the instance row");
    let second = sqlx::query("INSERT INTO smelt_instance (owns_unlabelled) VALUES (false)")
        .execute(&pool)
        .await;
    let refused = second.expect_err("a second instance row was accepted");
    assert!(
        refused.as_database_error().is_some_and(|d| d.is_unique_violation()),
        "refused for the wrong reason: {refused}"
    );
}


/// SME-99: the browser tier's ids start clear of every other run's,
/// for each table whose ids name cluster objects.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_ids_start_clear_of_other_runs(pool: PgPool) {
    let base = test_support::start_ids_clear_of_other_runs(&pool).await.expect("move the id sequences");
    assert!(base >= 2_000_000_000, "base {base} is in the range other runs use");

    let conversation = create_conversation(&pool).await.expect("conversation");
    let pod = create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    let volume = create_sandbox_volume(&pool, "sme-99-volume", "/data").await.expect("volume row");
    for (what, id) in [("conversation", conversation.id), ("sandbox pod", pod.id), ("sandbox volume", volume.id)] {
        assert!(id > base, "the next {what} id is {id}, not above the base {base}");
    }

    let again = test_support::start_ids_clear_of_other_runs(&pool).await.expect("move them again");
    assert_ne!(again, base, "two runs got the same base");
}

/// SME-255: `clear_ids_base()` never hands two callers the same base.
/// The gate flake: two near-simultaneous `#[sqlx::test]` workers drew the
/// same base, so the pod names they derive met in the shared test
/// namespace. 64 threads × 8 calls hammer the call the way the workers
/// do; every value must be distinct and inside the band `[2e9, 3e9)`
/// that names pods and claims.
#[test]
fn test_clear_ids_base_hands_no_two_callers_the_same_base() {
    let handles: Vec<_> = (0..64)
        .map(|_| {
            std::thread::spawn(|| -> Vec<i64> {
                (0..8).map(|_| crate::db::git::clear_ids_base()).collect()
            })
        })
        .collect();
    let bases: Vec<i64> = handles.into_iter().flat_map(|h| h.join().expect("the id thread")).collect();
    assert_eq!(bases.len(), 512, "64 threads × 8 calls");
    for base in &bases {
        assert!((2_000_000_000..3_000_000_000).contains(base), "base {base} is outside the band that names pods and claims");
    }
    let distinct: std::collections::HashSet<i64> = bases.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        512,
        "the set holds fewer than 512 values; two near-simultaneous calls returned the same base"
    );
}

/// SME-255: back-to-back harness moves never repeat and never crowd. The
/// test path moves the id sequences on every `own_sandbox` and ~80 test
/// setups; consecutive moves must sit at least `id_alloc::ID_STRIDE`
/// (100_000) apart, so one test's first ids can't reach the next
/// caller's. (The old fold drew each base from that call's own clock
/// tick: consecutive calls differed only by the clock ticks between two
/// reads, and the modulo wrapped downward about once a second.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_harness_moves_never_repeat_or_crowd(pool: PgPool) {
    let mut previous: Option<i64> = None;
    for round in 0..200 {
        let base = test_support::start_ids_clear_of_other_runs(&pool).await.expect("move the id sequences");
        if let Some(last) = previous {
            assert!(
                base >= last + 100_000,
                "round {round}: a harness move landed at {base}, less than 100_000 above the previous {last}"
            );
        }
        previous = Some(base);
    }
}
