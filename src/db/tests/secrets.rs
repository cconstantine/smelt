//! `named_secrets` (SME-242, T3): the schema's own promises, aimed at
//! the raw table — a caller that skips the API's validation still can't
//! duplicate a name, take the built-in's, or store a name or a value the
//! pod could not use.

use super::*;

async fn insert(pool: &PgPool, name: &str, value: &str) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query("INSERT INTO named_secrets (name, value) VALUES ($1, $2)")
        .bind(name)
        .bind(value)
        .execute(pool)
        .await?
        .rows_affected())
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_named_secrets_round_trip_and_refuse_a_second_of_a_name(pool: PgPool) {
    let pool = &pool;
    assert!(list_named_secrets(pool).await.expect("list").is_empty());
    // One transaction, so `now()` is one timestamp and the list's
    // name tie-break is what the assertion pins (a run apart in time
    // would order by age, and flake).
    let mut tx = pool.begin().await.expect("begin");
    sqlx::query("INSERT INTO named_secrets (name, value) VALUES ('linear-key', 'v1')")
        .execute(&mut *tx)
        .await
        .expect("insert");
    sqlx::query("INSERT INTO named_secrets (name, value) VALUES ('grafana-token', 'v2')")
        .execute(&mut *tx)
        .await
        .expect("insert");
    tx.commit().await.expect("commit");
    let listed = list_named_secrets(pool).await.expect("list");
    let names: Vec<&str> = listed.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["grafana-token", "linear-key"], "same age: by name");
    let linear = listed.iter().find(|s| s.name == "linear-key").expect("linear-key");
    assert_eq!(linear.value, "v1", "the value reads back whole");
    let dupe = insert(pool, "linear-key", "other").await.expect_err("unique name");
    assert!(dupe.to_string().contains("named_secrets_name_key"), "{dupe}");
}

/// Both CHECKs, from the database: 65 characters and the built-in's own
/// name refuse at the row, so no writer — API or not — can make a secret
/// shadow `git-ssh` or name a file no install could prune. The empty
/// value goes too: an empty secret has no meaning to grant.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_schema_refuses_a_bad_name_and_the_built_in_name(pool: PgPool) {
    let pool = &pool;
    let too_long = "a".repeat(65);
    assert!(insert(pool, &too_long, "v").await.is_err(), "65 characters");
    assert!(insert(pool, "git-ssh", "v").await.is_err(), "the built-in's name");
    assert!(insert(pool, "has space", "v").await.is_err(), "a space is not a file name");
    assert!(insert(pool, "", "v").await.is_err(), "an empty name");
    assert!(insert(pool, "ok-name", "").await.is_err(), "an empty value");
    assert!(list_named_secrets(pool).await.expect("list").is_empty(), "none of them landed");
}
