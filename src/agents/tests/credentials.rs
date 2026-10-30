//! SME-242, T8 (the notice half): what a start tells its parent about
//! credentials its persona asked for and cannot get — a name the
//! store doesn't back, and a `git-ssh` the chain doesn't have — read
//! against the real store and the real column. The `list_personas`
//! note joins these rows with commit 7's file.

use super::*;

/// A persona snapshot as `snapshot` writes it: `smelt.credentials`
/// verbatim under `credentials`, model and tools null.
fn persona_snapshot(credentials: Option<&[&str]>) -> db::PersonaSnapshot {
    let list = credentials.map(|names| {
        serde_json::Value::Array(names.iter().map(|name| serde_json::Value::String(name.to_string())).collect())
    });
    db::PersonaSnapshot {
        name: "driver".to_string(),
        path: "/workspace/smelt/.agents/personas/driver.md".to_string(),
        commit: Some("c0ffee".to_string()),
        body: "Drive.".to_string(),
        settings: serde_json::json!({ "model": null, "tools": null, "credentials": list }),
    }
}

/// A starting child of `root` with the chain column as given — what
/// `create_child` writes, before any persona has narrowed it.
async fn starting_child(pool: &PgPool, root: i64, credentials: &[&str]) -> i64 {
    let child = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('c', $1) RETURNING id")
        .bind(root)
        .fetch_one(pool)
        .await
        .expect("a child");
    sqlx::query(
        "INSERT INTO agents (conversation_id, root_id, depth, brief, state, credentials)
         VALUES ($1, $1, 1, 'b', 'starting', $2)",
    )
    .bind(child)
    .bind(credentials.iter().map(|name| name.to_string()).collect::<Vec<_>>())
    .execute(pool)
    .await
    .expect("its agent row");
    child
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_notice_names_the_credentials_the_start_cannot_give(pool: PgPool) {
    let root = db::create_conversation(&pool).await.expect("a root").id;
    sqlx::query("INSERT INTO named_secrets (name, value) VALUES ('linear-key', 'v')")
        .execute(&pool)
        .await
        .expect("a stored secret");

    // A persona whose every name is granted: nothing to report.
    let child = starting_child(&pool, root, &["git-ssh", "linear-key"]).await;
    let snapshot = persona_snapshot(Some(&["git-ssh", "linear-key"]));
    assert!(credentials_notes(&pool, child, &snapshot).await.is_empty(), "granted all round, said nothing");

    // A name the store doesn't back: the parent hears the store is
    // short, which under Rule S is the only way this happens — no
    // ancestor is ever named.
    let child = starting_child(&pool, root, &["git-ssh"]).await;
    let snapshot = persona_snapshot(Some(&["unstored-key"]));
    let notes = credentials_notes(&pool, child, &snapshot).await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains("`unstored-key`") && notes[0].contains("isn't one of your stored secrets"),
        "{}",
        notes[0]
    );

    // A `git-ssh` ask the chain doesn't have, said as the chain's
    // answer — and asked before the narrowing, which is what makes
    // the ask denied.
    let child = starting_child(&pool, root, &[]).await;
    let snapshot = persona_snapshot(Some(&["git-ssh"]));
    let notes = credentials_notes(&pool, child, &snapshot).await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("`git-ssh`") && notes[0].contains("chain doesn't have"), "{}", notes[0]);

    // Both at once: two lines, one per ask.
    let child = starting_child(&pool, root, &[]).await;
    let snapshot = persona_snapshot(Some(&["git-ssh", "deep-key"]));
    let notes = credentials_notes(&pool, child, &snapshot).await;
    assert_eq!(notes.len(), 2, "{notes:?}");

    // A NULL column — a pre-ticket row — falls open, so its persona's
    // `git-ssh` ask is granted and goes unsaid.
    let child = starting_child(&pool, root, &[]).await;
    sqlx::query("UPDATE agents SET credentials = NULL WHERE conversation_id = $1")
        .bind(child)
        .execute(&pool)
        .await
        .expect("a pre-ticket row");
    let snapshot = persona_snapshot(Some(&["git-ssh"]));
    assert!(credentials_notes(&pool, child, &snapshot).await.is_empty(), "NULL falls open, nothing to say");

    // And a persona with no `credentials` key at all — Rule G inherits
    // and Rule S names nothing — says nothing either.
    let child = starting_child(&pool, root, &[]).await;
    assert!(credentials_notes(&pool, child, &persona_snapshot(None)).await.is_empty(), "no key, no notes");
}
