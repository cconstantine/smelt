use super::*;

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_create_sandbox_volume_round_trips_name_and_mount_path(pool: PgPool) {
    let created = create_sandbox_volume(&pool, "ssh-key", "/home/sandbox/.ssh")
        .await
        .expect("create sandbox volume");
    assert_eq!(created.name, "ssh-key");
    assert_eq!(created.mount_path, "/home/sandbox/.ssh");

    let fetched = get_sandbox_volume(&pool, created.id)
        .await
        .expect("get sandbox volume")
        .expect("should exist");
    assert_eq!(fetched, created);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_get_sandbox_volume_returns_none_for_unknown_id(pool: PgPool) {
    let result = get_sandbox_volume(&pool, 999_999)
        .await
        .expect("query should succeed");
    assert!(result.is_none());
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_list_sandbox_volumes_orders_by_name(pool: PgPool) {
    create_sandbox_volume(&pool, "zzz-cache", "/data/cache")
        .await
        .expect("create volume");
    create_sandbox_volume(&pool, "aaa-ssh", "/home/sandbox/.ssh")
        .await
        .expect("create volume");

    let listed = list_sandbox_volumes(&pool)
        .await
        .expect("list sandbox volumes");
    let names: Vec<&str> = listed.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, vec!["aaa-ssh", "zzz-cache"]);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_delete_sandbox_volume_actually_removes_it(pool: PgPool) {
    let created = create_sandbox_volume(&pool, "build-cache", "/data/cache")
        .await
        .expect("create volume");
    delete_sandbox_volume(&pool, created.id)
        .await
        .expect("delete sandbox volume");

    let fetched = get_sandbox_volume(&pool, created.id)
        .await
        .expect("query should succeed");
    assert!(
        fetched.is_none(),
        "deleted volume should no longer be gettable"
    );
}
