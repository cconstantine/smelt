//! The volume tools' refusals (SME-239): every one of them comes before the
//! cluster is reached, so they run against a database alone. The paths that
//! need a claim — a volume created, a volume deleted while a pod holds it —
//! are in `sandbox`'s `pod_lifecycle_tests`, on the real cluster.

use super::*;

/// A conversation with nothing mounted and no pod.
async fn conversation(pool: &PgPool) -> i64 {
    db::create_conversation(pool).await.expect("create conversation").id
}

async fn call(pool: &PgPool, conversation_id: i64, tool: &str, input: serde_json::Value) -> Result<String, String> {
    crate::anthropic::tools::execute(pool, conversation_id, "toolu_volume", tool, &input).await
}

async fn row_counts(pool: &PgPool) -> (i64, i64) {
    let volumes = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agent_volumes")
        .fetch_one(pool)
        .await
        .expect("count volumes");
    let mounts = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM conversation_volume_mounts")
        .fetch_one(pool)
        .await
        .expect("count mounts");
    (volumes, mounts)
}

/// A `sandbox_volumes` row, written by hand: the user's volumes page isn't
/// this ticket's, but its paths are what a model volume must never shadow.
async fn global_volume(pool: &PgPool, name: &str, mount_path: &str) {
    db::create_sandbox_volume(pool, name, mount_path).await.expect("create global volume");
}

/// A volume another conversation created and passed to `holder`, both mount
/// rows in place. Returns (creator, volume id).
async fn passed_volume(pool: &PgPool, holder: i64, name: &str, mount_path: &str) -> (i64, i64) {
    let owner = db::create_conversation(pool).await.expect("create owner").id;
    let volume = sqlx::query_scalar::<_, i64>(
        "INSERT INTO agent_volumes (owner_conversation_id, name, mount_path) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(owner)
    .bind(name)
    .bind(mount_path)
    .fetch_one(pool)
    .await
    .expect("insert volume");
    for conversation_id in [owner, holder] {
        sqlx::query("INSERT INTO conversation_volume_mounts (conversation_id, volume_id) VALUES ($1, $2)")
            .bind(conversation_id)
            .bind(volume)
            .execute(pool)
            .await
            .expect("insert mount");
    }
    (owner, volume)
}

/// This conversation's own volume and mount row, by hand: writing them is
/// `create_volume`'s job, and this file's refusals are reached without one.
async fn own_volume(pool: &PgPool, owner: i64, name: &str, mount_path: &str) -> i64 {
    let volume = sqlx::query_scalar::<_, i64>(
        "INSERT INTO agent_volumes (owner_conversation_id, name, mount_path) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(owner)
    .bind(name)
    .bind(mount_path)
    .fetch_one(pool)
    .await
    .expect("insert volume");
    sqlx::query("INSERT INTO conversation_volume_mounts (conversation_id, volume_id) VALUES ($1, $2)")
        .bind(owner)
        .bind(volume)
        .execute(pool)
        .await
        .expect("insert mount");
    volume
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_create_volume_refuses_every_input_it_would_write_nothing_for(pool: PgPool) {
    let conversation = conversation(&pool).await;
    global_volume(&pool, "shared", "/data/shared").await;
    let (owner, _) = passed_volume(&pool, conversation, "cache", "/data/cache").await;

    // Each is (the input, what its refusal has to say). Every one of these is
    // answered before the cluster is reached and before a row is written, so
    // the count below is the point of the test as much as the wording is.
    let refusals: Vec<(serde_json::Value, String)> = vec![
        (serde_json::json!({"name": "my cache", "mount_path": "/d"}), "1-64 letters".into()),
        (serde_json::json!({"name": "", "mount_path": "/d"}), "1-64 letters".into()),
        (serde_json::json!({"name": "x".repeat(65), "mount_path": "/d"}), "1-64 letters".into()),
        (serde_json::json!({"name": "x!".repeat(20), "mount_path": "/d"}), "1-64 letters".into()),
        (serde_json::json!({"mount_path": "/d"}), "name is required".into()),
        (serde_json::json!({"name": "x"}), "mount_path is required".into()),
        (serde_json::json!({"name": "x", "mount_path": "cache"}), "absolute path".into()),
        (serde_json::json!({"name": "x", "mount_path": "/data/../etc"}), "must not contain ..".into()),
        (serde_json::json!({"name": "x", "mount_path": "/"}), "root directory".into()),
        (serde_json::json!({"name": "x", "mount_path": "/workspace/repo"}), "at or under \"/workspace\"".into()),
        (serde_json::json!({"name": "x", "mount_path": "/etc/smelt"}), "at or under \"/etc/smelt\"".into()),
        (serde_json::json!({"name": "x", "mount_path": "/run/docker-sock"}), "at or under \"/run/docker-sock\"".into()),
        (serde_json::json!({"name": "x", "mount_path": "/usr/local"}), "at or under \"/usr\"".into()),
        (serde_json::json!({"name": "x", "mount_path": "/data/shared"}), "the global volume \"shared\" mounts".into()),
        (serde_json::json!({"name": "other", "mount_path": "/data/cache"}), "already mount a volume at \"/data/cache\"".into()),
        (serde_json::json!({"name": "cache", "mount_path": "/data/new"}), format!("created by conversation #{owner}")),
    ];
    for (input, phrase) in &refusals {
        let error = call(&pool, conversation, "create_volume", input.clone()).await.expect_err("a refusal was expected");
        assert!(error.contains(phrase), "{input} was refused with {error:?}, which doesn't say {phrase:?}");
    }
    assert_eq!(row_counts(&pool).await, (1, 2), "only the volume another conversation passed in, written by hand");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_delete_volume_refuses_what_it_didnt_create_and_what_is_still_mounted(pool: PgPool) {
    let conversation = conversation(&pool).await;
    let (owner, _) = passed_volume(&pool, conversation, "cache", "/data/cache").await;

    let error = call(&pool, conversation, "delete_volume", serde_json::json!({"name": "cache"}))
        .await
        .expect_err("a holder can't delete what it was passed");
    assert!(error.contains(&format!("created by conversation #{owner}")), "{error}");
    assert!(error.contains("only that conversation can delete it"), "{error}");

    let error = call(&pool, conversation, "delete_volume", serde_json::json!({"name": "nope"}))
        .await
        .expect_err("nothing of that name exists");
    assert!(error.contains("no volume named \"nope\" that you created"), "{error}");

    let error = call(&pool, conversation, "delete_volume", serde_json::json!({}))
        .await
        .expect_err("no name given");
    assert!(error.contains("name is required"), "{error}");

    // Its own volume, mounted by a child that's still working: refused, the
    // child named by conversation and state, and nothing written.
    let volume = own_volume(&pool, conversation, "mine", "/data/mine").await;
    let child = db::create_conversation(&pool).await.expect("create child").id;
    sqlx::query("INSERT INTO agents (conversation_id, root_id, depth, brief, state) VALUES ($1, $1, 2, 'b', $2)")
        .bind(child)
        .bind(db::AGENT_ACTIVE)
        .execute(&pool)
        .await
        .expect("insert the child's agent row");
    sqlx::query("INSERT INTO conversation_volume_mounts (conversation_id, volume_id) VALUES ($1, $2)")
        .bind(child)
        .bind(volume)
        .execute(&pool)
        .await
        .expect("mount it in the child");

    let error = call(&pool, conversation, "delete_volume", serde_json::json!({"name": "mine"}))
        .await
        .expect_err("a working holder refuses the delete");
    assert!(error.contains(&format!("conversation #{child} (active)")), "{error}");
    assert!(error.contains("stop or finish"), "{error}");
    assert_eq!(row_counts(&pool).await, (2, 4), "the refusal writes nothing");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_delete_volume_takes_its_rows_even_when_no_claim_can_be_reached(pool: PgPool) {
    // No cluster in this test: the rows are the record and the claim is left
    // for the startup sweep — the design's answer to a delete cut off
    // part-way, so this is the behaviour rather than a test artefact.
    let conversation = conversation(&pool).await;
    own_volume(&pool, conversation, "mine", "/data/mine").await;

    let result = call(&pool, conversation, "delete_volume", serde_json::json!({"name": "mine"}))
        .await
        .expect("the rows go whatever the claim does");
    assert!(result.contains("volume \"mine\" deleted"), "{result}");
    assert!(result.contains("Nothing mounts it now."), "{result}");
    assert_eq!(row_counts(&pool).await, (0, 0), "the row and its mount row are gone");
}

/// The sentence each tool ends with: the design's "the tool's result says so"
/// for a volume that can only mount at the next pod start.
#[test]
fn test_the_volume_results_say_when_a_mount_takes_effect() {
    assert_eq!(create_sentence(false), "It mounts there when your pod next starts.");
    assert!(create_sentence(true).contains("can't add a volume to a live pod"), "{}", create_sentence(true));
    assert!(create_sentence(true).contains("terminate_pod then create_pod"), "{}", create_sentence(true));
    assert!(delete_sentence(true).contains("until its next start"), "{}", delete_sentence(true));
    assert_eq!(delete_sentence(false), "Nothing mounts it now.");
}
