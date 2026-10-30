//! SSH keys and the commit identity: creation, one-at-a-time, import, delete.

use super::*;
use crate::credentials::install_into_new_pod;

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_create_key_stores_it_and_returns_only_the_public_half(pool: PgPool) {
    let created = create_key(&pool, "github").await.expect("create key");
    assert_eq!(created.name, "github");
    assert!(created.public_key.starts_with("ssh-ed25519 "), "{}", created.public_key);
    assert_eq!(created.fingerprint, fingerprint(&created.public_key).expect("fp"));

    let stored = db::list_ssh_keys(&pool).await.expect("list");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].public_key, created.public_key);
    assert!(stored[0].private_key.contains("OPENSSH PRIVATE KEY"));
    assert_eq!(list_keys(&pool).await.expect("list keys"), vec![created]);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_create_key_refuses_an_unsafe_name(pool: PgPool) {
    let unsafe_name = create_key(&pool, "../x").await.expect_err("unsafe");
    assert!(unsafe_name.contains("letters"), "{unsafe_name}");
    assert!(db::list_ssh_keys(&pool).await.expect("list").is_empty());
}

/// One key at a time, for now: with several, the host picks the
/// first it knows, which breaks per-repo deploy keys (SME-32 code
/// review 3).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_only_one_key_at_a_time(pool: PgPool) {
    let first = create_key(&pool, "github").await.expect("first");
    let generated = create_key(&pool, "another").await.expect_err("a second key");
    assert!(generated.contains("Delete it first"), "{generated}");
    let imported = import_key_named(&pool, "imported", &generate_key("x").private_key)
        .await
        .expect_err("a second key, imported");
    assert!(imported.contains("Delete it first"), "{imported}");
    // The database refuses one too, whatever the code checks.
    assert!(db::create_ssh_key(&pool, "sneaky", "pub", "priv").await.is_err());
    assert_eq!(list_keys(&pool).await.expect("list").len(), 1);

    delete_key(&pool, first.id).await.expect("delete");
    create_key(&pool, "replacement").await.expect("a key again once there's none");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_import_key_stores_the_derived_public_half(pool: PgPool) {
    let pair = generate_key("elsewhere");
    let imported = import_key_named(&pool, "laptop", &pair.private_key)
        .await
        .expect("import");
    assert_eq!(imported.name, "laptop");
    assert_eq!(imported.public_key, pair.public_key);
    let refused = import_key_named(&pool, "bad", "nope").await.expect_err("garbage");
    assert!(refused.contains("private key"), "{refused}");
}

/// No key and no identity: a new pod is left alone (SME-32 code
/// review 8, finding 2). Pod 999999 doesn't exist, and this test
/// process has no cluster: any attempt to write would fail.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_new_pod_gets_nothing_when_there_is_nothing_to_install(pool: PgPool) {
    install_into_new_pod(&pool, 999_999).await.expect("nothing to do");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_delete_key_and_save_identity(pool: PgPool) {
    let key = create_key(&pool, "gone").await.expect("create");
    delete_key(&pool, key.id).await.expect("delete");
    assert!(list_keys(&pool).await.expect("list").is_empty());

    let identity = GitIdentity {
        name: "Ada".into(),
        email: "ada@example.com".into(),
    };
    save_identity(&pool, &identity).await.expect("save");
    assert_eq!(db::get_git_identity(&pool).await.expect("get"), identity);
    let bad = GitIdentity {
        name: "Ada\nEvil".into(),
        email: "ada@example.com".into(),
    };
    assert!(save_identity(&pool, &bad).await.is_err());
    assert_eq!(db::get_git_identity(&pool).await.expect("get"), identity);
}
