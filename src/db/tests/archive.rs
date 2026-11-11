//! SME-480: archiving a top-level conversation. Its agents follow it, so
//! the tree reads each node's root.

use super::*;

/// A child of `caller` made by `create_child`, the way `start_agent` makes
/// one, so its `agents.root_id` is the real one.
async fn child(pool: &PgPool, caller: i64) -> i64 {
    let new = NewChild { caller, brief: "b", persona_name: None, model: None, repo: None, max_depth: 5, volumes: &[] };
    let CreateChild::Created { child, .. } = create_child(pool, &new).await.expect("create_child") else {
        panic!("a caller below the ceiling starts one");
    };
    child
}

/// Marks `id` archived by hand, two hours ago.
async fn archive_by_hand(pool: &PgPool, id: i64) -> chrono::NaiveDateTime {
    sqlx::query_scalar::<_, chrono::NaiveDateTime>(
        "UPDATE conversations SET archived_at = now() - interval '2 hours' WHERE id = $1 RETURNING archived_at",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("archive by hand")
}

/// The row's `(archived_at, updated_at)`.
async fn marks(pool: &PgPool, id: i64) -> (Option<chrono::NaiveDateTime>, chrono::NaiveDateTime) {
    sqlx::query_as::<_, (Option<chrono::NaiveDateTime>, chrono::NaiveDateTime)>(
        "SELECT archived_at, updated_at FROM conversations WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("the row")
}

/// T1: archive sets the mark on a root and keeps `updated_at`; a second
/// archive keeps the first time; unarchive clears it, twice over; a
/// missing id is `NotFound` either way.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_archive_marks_a_root_once_and_unarchive_clears_it(pool: PgPool) {
    let root = test_conversation(&pool).await.id;
    let (_, updated) = marks(&pool, root).await;

    let ArchiveOutcome::Done(done) = set_archived(&pool, root, true).await.expect("archive") else {
        panic!("a root is archived");
    };
    let (first, after) = marks(&pool, root).await;
    assert!(first.is_some(), "the mark is set");
    assert_eq!(done.archived_at, first, "the returned row shows it");
    assert_eq!(after, updated, "updated_at is untouched (D5)");

    // A second archive, a moment later, keeps the first time.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let again = set_archived(&pool, root, true).await.expect("archive again");
    assert!(matches!(&again, ArchiveOutcome::Done(c) if c.archived_at == first), "{again:?}");
    assert_eq!(marks(&pool, root).await.0, first);

    for _ in 0..2 {
        let ArchiveOutcome::Done(done) = set_archived(&pool, root, false).await.expect("unarchive") else {
            panic!("a root is unarchived");
        };
        assert_eq!(done.archived_at, None);
        assert_eq!(marks(&pool, root).await, (None, updated), "cleared, updated_at still untouched");
    }

    for archived in [true, false] {
        assert_eq!(set_archived(&pool, 987_654_321, archived).await.expect("a missing id"), ArchiveOutcome::NotFound);
    }
}

/// T2: an agent, at depth 1 or 2, is refused with its root's id, and its
/// row is left as it was.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_agent_is_not_archived_and_names_its_root(pool: PgPool) {
    let root = test_conversation(&pool).await.id;
    let kid = child(&pool, root).await;
    let grandkid = child(&pool, kid).await;
    for id in [kid, grandkid] {
        let before = marks(&pool, id).await;
        for archived in [true, false] {
            assert_eq!(
                set_archived(&pool, id, archived).await.expect("set_archived"),
                ArchiveOutcome::NotTopLevel { root_id: root },
                "agent {id}"
            );
        }
        assert_eq!(marks(&pool, id).await, before, "the agent's row is untouched");
    }
    assert_eq!(marks(&pool, root).await.0, None, "nor is its root archived by the refusal");
}

/// T3: the tree gives a root's `archived_at` to its child and grandchild
/// (depth 2) and leaves another tree's nodes null.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_tree_gives_every_node_its_roots_archived_at(pool: PgPool) {
    let root = test_conversation(&pool).await.id;
    let kid = child(&pool, root).await;
    let grandkid = child(&pool, kid).await;
    let other = test_conversation(&pool).await.id;
    let other_kid = child(&pool, other).await;
    let at = archive_by_hand(&pool, root).await;

    let rows = conversation_tree(&pool).await.expect("the tree");
    let archived = |id: i64| rows.iter().find(|r| r.id == id).expect("listed").archived_at;
    assert_eq!(archived(root), Some(at), "the root's own mark");
    assert_eq!(archived(kid), Some(at), "a child follows its root");
    assert_eq!(archived(grandkid), Some(at), "so does a grandchild, depth 2");
    assert_eq!((archived(other), archived(other_kid)), (None, None), "another tree is untouched");
}
