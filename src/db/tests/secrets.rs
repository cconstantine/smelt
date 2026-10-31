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

/// SME-334 T4: the column's pin. Raw inserts holding `\n`, `\n\n`, a
/// trailing `\n` and `\r\n` bytes read back byte-equal from every read
/// path, and `set_named_secret_value` carries a multi-line replacement
/// the same way. `TEXT` stores what it is given — this is what makes
/// the store innocent and the form the only suspect. The `\r\n` row
/// says the quiet part: smelt keeps CR bytes handed to it directly
/// (the API path), and it is only the browser that rewrites CRLF to LF
/// on the paste path (the plan's documented asymmetry).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_multi_line_value_round_trips_byte_for_byte(pool: PgPool) {
    let pool = &pool;
    let values = [
        ("lf-lines", "a\nb"),
        ("blank-line", "a\n\nb"),
        ("trailing-nl", "a\nb\n"),
        ("crlf-lines", "a\r\nb\r\n"),
    ];
    for (name, value) in values {
        insert(pool, name, value).await.expect("insert");
    }
    let listed = list_named_secrets(pool).await.expect("list");
    for (name, value) in values {
        let found = listed.iter().find(|s| s.name == name).expect(name);
        assert_eq!(found.value, *value, "{name} came back changed in the list");
        // Every read path, both SELECTs: the CRLF row above all — the
        // list alone would leave `get_named_secret` untried by it.
        let got = get_named_secret(pool, found.id).await.expect("get").expect(name);
        assert_eq!(got.value, *value, "{name} came back changed by id");
    }

    // The change path is the same story: whole bytes in, whole bytes back.
    let changed = listed.iter().find(|s| s.name == "trailing-nl").expect("row");
    let replacement = "apiVersion: v1\nclusters:\n\nkind: list\n";
    assert!(set_named_secret_value(pool, changed.id, replacement).await.expect("set"));
    let again = get_named_secret(pool, changed.id).await.expect("get").expect("row after the change");
    assert_eq!(again.value, replacement, "a multi-line replacement round-trips");
}
