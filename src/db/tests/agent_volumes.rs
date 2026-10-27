//! Volumes the model creates (SME-239): what the migration's two tables
//! promise on their own, before the tool that writes them exists.

use super::*;

/// A volume row, put in by hand: the transaction that writes the row and its
/// owner's mount row together arrives with the `create_volume` tool.
async fn insert_volume(pool: &PgPool, owner: i64, name: &str, mount_path: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO agent_volumes (owner_conversation_id, name, mount_path) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(owner)
    .bind(name)
    .bind(mount_path)
    .fetch_one(pool)
    .await
    .expect("insert volume")
}

async fn insert_mount(pool: &PgPool, conversation_id: i64, volume_id: i64) {
    sqlx::query("INSERT INTO conversation_volume_mounts (conversation_id, volume_id) VALUES ($1, $2)")
        .bind(conversation_id)
        .bind(volume_id)
        .execute(pool)
        .await
        .expect("insert mount");
}

async fn mount_rows(pool: &PgPool, volume_id: i64) -> Vec<i64> {
    sqlx::query_scalar::<_, i64>(
        "SELECT conversation_id FROM conversation_volume_mounts WHERE volume_id = $1 ORDER BY conversation_id",
    )
    .bind(volume_id)
    .fetch_all(pool)
    .await
    .expect("mount rows")
}

async fn volume_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agent_volumes")
        .fetch_one(pool)
        .await
        .expect("count volumes")
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_deleting_the_creating_conversation_takes_the_volume_and_every_mount_row(pool: PgPool) {
    let owner = test_conversation(&pool).await;
    let holder = test_conversation(&pool).await;
    let volume = insert_volume(&pool, owner.id, "cache", "/data/cache").await;
    insert_mount(&pool, owner.id, volume).await;
    insert_mount(&pool, holder.id, volume).await;

    sqlx::query("DELETE FROM conversations WHERE id = $1")
        .bind(owner.id)
        .execute(&pool)
        .await
        .expect("delete the creating conversation");

    assert_eq!(volume_rows(&pool).await, 0, "the volume outlived its creator");
    assert_eq!(mount_rows(&pool, volume).await, Vec::<i64>::new(), "and so did every conversation mounting it");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_deleting_a_conversation_holding_a_passed_volume_keeps_the_volume(pool: PgPool) {
    let owner = test_conversation(&pool).await;
    let holder = test_conversation(&pool).await;
    let volume = insert_volume(&pool, owner.id, "cache", "/data/cache").await;
    insert_mount(&pool, owner.id, volume).await;
    insert_mount(&pool, holder.id, volume).await;

    sqlx::query("DELETE FROM conversations WHERE id = $1")
        .bind(holder.id)
        .execute(&pool)
        .await
        .expect("delete the conversation holding it");

    assert_eq!(volume_rows(&pool).await, 1, "a volume belongs to whoever created it, not to whoever mounts it");
    assert_eq!(mount_rows(&pool, volume).await, vec![owner.id], "only the holder's mount row went");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_list_conversation_volume_mounts_returns_own_and_passed_not_a_siblings(pool: PgPool) {
    let parent = test_conversation(&pool).await;
    let child = test_conversation(&pool).await;
    let sibling = test_conversation(&pool).await;
    let passed = insert_volume(&pool, parent.id, "shared", "/data/shared").await;
    insert_mount(&pool, parent.id, passed).await;
    insert_mount(&pool, child.id, passed).await;
    let own = insert_volume(&pool, child.id, "own", "/data/own").await;
    insert_mount(&pool, child.id, own).await;
    let theirs = insert_volume(&pool, sibling.id, "theirs", "/data/theirs").await;
    insert_mount(&pool, sibling.id, theirs).await;

    let mounts = list_conversation_volume_mounts(&pool, child.id).await.expect("list child mounts");
    assert_eq!(
        mounts.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
        vec!["shared", "own"],
        "the volume it was passed and the one it created, oldest first"
    );
    assert_eq!(mounts[0].owner_conversation_id, parent.id, "the prompt's ownership line reads this");
    assert_eq!(mounts[0].mount_path, "/data/shared", "the pod spec's mount path reads this");
    assert_eq!(mounts[1].id, own);

    let parent_mounts = list_conversation_volume_mounts(&pool, parent.id).await.expect("list parent mounts");
    assert_eq!(parent_mounts.iter().map(|v| v.id).collect::<Vec<_>>(), vec![passed], "the creator sees its own volume, not its child's");
    assert_eq!(list_conversation_volume_mounts(&pool, sibling.id).await.expect("list sibling mounts").len(), 1);
    let nobody = test_conversation(&pool).await;
    assert!(list_conversation_volume_mounts(&pool, nobody.id).await.expect("list empty").is_empty());
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_create_agent_volume_with_owner_mount_writes_the_row_and_the_owners_mount_row(pool: PgPool) {
    let owner = test_conversation(&pool).await;
    let mut tx = pool.begin().await.expect("begin");
    let volume = create_agent_volume_with_owner_mount(&mut *tx, owner.id, "cache", "/data/cache")
        .await
        .expect("create");
    tx.commit().await.expect("commit");

    assert_eq!(volume.owner_conversation_id, owner.id);
    assert_eq!(volume.name, "cache");
    assert_eq!(volume.mount_path, "/data/cache");
    assert_eq!(
        agent_volume_held_by(&pool, owner.id, "cache").await.expect("held").expect("the volume"),
        volume,
        "the creator holds what it made"
    );
    assert_eq!(mount_rows(&pool, volume.id).await, vec![owner.id], "and its mount row is the owner's own");

    // The migration's UNIQUE is the backstop for the tool's own name check.
    let mut tx = pool.begin().await.expect("begin");
    assert!(create_agent_volume_with_owner_mount(&mut *tx, owner.id, "cache", "/other").await.is_err());
    drop(tx);
    // The same name for another conversation is a different volume.
    let other = test_conversation(&pool).await;
    let mut tx = pool.begin().await.expect("begin");
    let theirs = create_agent_volume_with_owner_mount(&mut *tx, other.id, "cache", "/data/cache")
        .await
        .expect("the same name, another owner");
    tx.commit().await.expect("commit");
    assert_ne!(theirs.id, volume.id);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_agent_volume_held_by_and_volume_mounted_at_reach_a_passed_volume_too(pool: PgPool) {
    let parent = test_conversation(&pool).await;
    let child = test_conversation(&pool).await;
    let volume = insert_volume(&pool, parent.id, "cache", "/data/cache").await;
    insert_mount(&pool, parent.id, volume).await;
    insert_mount(&pool, child.id, volume).await;

    let held = agent_volume_held_by(&pool, child.id, "cache").await.expect("held").expect("the passed volume");
    assert_eq!(held.id, volume);
    assert_eq!(held.owner_conversation_id, parent.id, "the holder didn't create it, which delete_volume reads");
    assert!(agent_volume_held_by(&pool, child.id, "nope").await.expect("query").is_none());
    assert!(
        volume_mounted_at(&pool, child.id, "/data/cache").await.expect("mounted"),
        "the path is taken in the holder's pod too, by a volume it didn't make"
    );
    assert!(!volume_mounted_at(&pool, child.id, "/data/other").await.expect("query"));
    assert!(!volume_mounted_at(&pool, parent.id, "/data/other").await.expect("query"));
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_delete_agent_volume_if_unused_takes_the_row_and_every_mount_row(pool: PgPool) {
    let owner = test_conversation(&pool).await;
    let holder = test_conversation(&pool).await;
    let volume = insert_volume(&pool, owner.id, "cache", "/data/cache").await;
    insert_mount(&pool, owner.id, volume).await;
    insert_mount(&pool, holder.id, volume).await;

    let deleted = delete_agent_volume_if_unused(&pool, owner.id, "cache").await.expect("delete");
    let volume = match deleted {
        DeleteAgentVolume::Deleted(volume) => volume,
        other => panic!("expected the delete, got {other:?}"),
    };
    assert_eq!(volume.mount_path, "/data/cache", "the claim delete reads the row's own path");
    assert_eq!(volume.owner_conversation_id, owner.id, "and who created it, which the claim's label names");
    assert_eq!(volume_rows(&pool).await, 0);
    assert_eq!(mount_rows(&pool, volume.id).await, Vec::<i64>::new(), "every mount row cascades with it");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_delete_agent_volume_if_unused_never_reaches_a_volume_it_was_passed(pool: PgPool) {
    let owner = test_conversation(&pool).await;
    let holder = test_conversation(&pool).await;
    let volume = insert_volume(&pool, owner.id, "cache", "/data/cache").await;
    insert_mount(&pool, owner.id, volume).await;
    insert_mount(&pool, holder.id, volume).await;

    assert_eq!(
        delete_agent_volume_if_unused(&pool, holder.id, "cache").await.expect("delete"),
        DeleteAgentVolume::NotFound,
        "a holder never gets past the creator filter, so its own mount row stays too"
    );
    assert_eq!(volume_rows(&pool).await, 1);
    assert_eq!(mount_rows(&pool, volume).await, vec![owner.id, holder.id]);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_delete_agent_volume_if_unused_refuses_while_a_holder_is_starting_or_active(pool: PgPool) {
    let owner = test_conversation(&pool).await;
    let working = test_conversation(&pool).await;
    let finished = test_conversation(&pool).await;
    let volume = insert_volume(&pool, owner.id, "cache", "/data/cache").await;
    for conversation_id in [owner.id, working.id, finished.id] {
        insert_mount(&pool, conversation_id, volume).await;
    }
    for (conversation_id, state) in [(working.id, AGENT_STARTING), (finished.id, AGENT_FINISHED)] {
        sqlx::query("INSERT INTO agents (conversation_id, root_id, depth, brief, state) VALUES ($1, $1, 1, 'b', $2)")
            .bind(conversation_id)
            .bind(state)
            .execute(&pool)
            .await
            .expect("insert agent");
    }

    assert_eq!(
        delete_agent_volume_if_unused(&pool, owner.id, "cache").await.expect("delete"),
        DeleteAgentVolume::HeldBy(vec![(working.id, AGENT_STARTING.to_string())]),
        "the finished holder doesn't hold it, and the owner's own agent state never does"
    );
    assert_eq!(volume_rows(&pool).await, 1, "the refusal writes nothing");

    sqlx::query("UPDATE agents SET state = $2 WHERE conversation_id = $1")
        .bind(working.id)
        .bind(AGENT_STOPPED)
        .execute(&pool)
        .await
        .expect("stop the holder");
    assert!(matches!(delete_agent_volume_if_unused(&pool, owner.id, "cache").await.expect("delete"),
        DeleteAgentVolume::Deleted(_)));
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_concurrent_pass_of_a_volume_and_its_delete_exclude_each_other(pool: PgPool) {
    // The pair the design locks for: `create_child` shares the volume row
    // while it records the child's mount row, `delete_volume` takes it
    // exclusive to check-then-delete. So one of the two refuses — either the
    // delete waits and then sees the child `starting`, or the rows are gone
    // before the child's lookup and it refuses. No child ends up mounted to a
    // volume that isn't there.
    let parent = test_conversation(&pool).await;
    let child = test_conversation(&pool).await;
    let volume = insert_volume(&pool, parent.id, "cache", "/data/cache").await;
    insert_mount(&pool, parent.id, volume).await;
    sqlx::query("INSERT INTO agents (conversation_id, root_id, depth, brief, state) VALUES ($1, $1, 2, 'b', $2)")
        .bind(child.id)
        .bind(AGENT_STARTING)
        .execute(&pool)
        .await
        .expect("insert the child's agent row");

    // The start's side: the lookup `create_child` runs, holding its share.
    let mut starting = pool.begin().await.expect("begin the start's transaction");
    let held = sqlx::query_as::<_, AgentVolume>(
        "SELECT v.id, v.owner_conversation_id, v.name, v.mount_path
           FROM conversation_volume_mounts m
           JOIN agent_volumes v ON v.id = m.volume_id
          WHERE m.conversation_id = $1 AND v.name = $2
          FOR SHARE OF v",
    )
    .bind(parent.id)
    .bind("cache")
    .fetch_all(&mut *starting)
    .await
    .expect("the start's volume lookup");
    assert_eq!(held.iter().map(|v| v.id).collect::<Vec<_>>(), vec![volume]);
    sqlx::query("INSERT INTO conversation_volume_mounts (conversation_id, volume_id) VALUES ($1, $2)")
        .bind(child.id)
        .bind(volume)
        .execute(&mut *starting)
        .await
        .expect("the child's mount row");

    // The delete can't get the row while that's open...
    let blocked = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        delete_agent_volume_if_unused(&pool, parent.id, "cache"),
    )
    .await;
    assert!(blocked.is_err(), "the delete waits for the start's share lock rather than racing it");

    // ...and once the start commits, the delete refuses: the child it just
    // mounted is `starting`.
    starting.commit().await.expect("commit the start");
    assert_eq!(
        delete_agent_volume_if_unused(&pool, parent.id, "cache").await.expect("delete"),
        DeleteAgentVolume::HeldBy(vec![(child.id, AGENT_STARTING.to_string())])
    );
    assert_eq!(volume_rows(&pool).await, 1);
    assert_eq!(mount_rows(&pool, volume).await, vec![parent.id, child.id]);
}
