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
