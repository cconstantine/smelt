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
