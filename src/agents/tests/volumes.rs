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
    // The copy answers for the claim too: it was never reached, so it isn't
    // gone — the startup sweep is what will delete it (A-F1).
    assert!(result.contains("its claim is left for the startup sweep"), "{result}");
    assert!(!result.contains("and its claim are gone"), "the claim didn't go: {result}");
    assert!(result.contains("Nothing mounts it now."), "{result}");
    assert_eq!(row_counts(&pool).await, (0, 0), "the row and its mount row are gone");
}

/// Which conversations mount `volume`, oldest first.
async fn holders(pool: &PgPool, volume: i64) -> Vec<i64> {
    sqlx::query_scalar::<_, i64>("SELECT conversation_id FROM conversation_volume_mounts WHERE volume_id = $1 ORDER BY conversation_id")
        .bind(volume)
        .fetch_all(pool)
        .await
        .expect("mount rows")
}

/// Passing a volume to a child (SME-239): by name only, only one the caller
/// holds, and the mount row it records is what the child's pod will mount.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_start_agent_refuses_a_volume_its_caller_doesnt_hold(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = conversation(&pool).await;

    let error = call(&pool, root, "start_agent", serde_json::json!({"brief": "work", "volumes": "cache"}))
        .await
        .expect_err("a list of names is asked for");
    assert!(error.contains("volumes must be an array of volume names"), "{error}");

    let error = call(&pool, root, "start_agent", serde_json::json!({"brief": "work", "volumes": ["nope"]}))
        .await
        .expect_err("a name nobody holds is refused");
    assert!(error.contains("you don't hold a volume named \"nope\""), "{error}");
    assert!(error.contains("create_volume makes one"), "{error}");

    let children = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM conversations WHERE parent_id = $1")
        .bind(root)
        .fetch_one(&pool)
        .await
        .expect("count children");
    assert_eq!(children, 0, "a refused start leaves no child");
    assert_eq!(row_counts(&pool).await, (0, 0), "nor does it write a mount row");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_start_agent_mounts_a_held_volume_on_the_child_and_names_it(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = conversation(&pool).await;
    let volume = own_volume(&pool, root, "cache", "/data/cache").await;

    // The same name twice in one call: the child mounts it once.
    let result = call(&pool, root, "start_agent", serde_json::json!({"brief": "work", "volumes": ["cache", "cache"]}))
        .await
        .expect("the caller holds the name");
    let child = child_of(&pool, root, &result).await;
    assert!(result.contains("cache"), "the reply names what it passed: {result}");
    assert_eq!(holders(&pool, volume).await, vec![root, child], "committed by the time the tool returns");

    // A passed volume is held, so the child can pass it on: a grandchild gets
    // its own mount row for the same one volume, and still can't delete it.
    let grand = call(&pool, child, "start_agent", serde_json::json!({"brief": "more work", "volumes": ["cache"]}))
        .await
        .expect("a volume passed down is held, so it is re-passable");
    let grandchild = child_of(&pool, child, &grand).await;
    assert_eq!(holders(&pool, volume).await, vec![root, child, grandchild], "one claim, three mounting pods");

    let error = call(&pool, grandchild, "start_agent", serde_json::json!({"brief": "deeper", "volumes": ["cache2"]}))
        .await
        .expect_err("the grandchild holds no such volume");
    assert!(error.contains("you don't hold a volume named \"cache2\""), "{error}");
    assert_eq!(holders(&pool, volume).await, vec![root, child, grandchild], "and nothing changed");
}

/// The sentence each tool ends with: the design's "the tool's result says so"
/// for a volume that can only mount at the next pod start, and — for a delete
/// whose claim never could be deleted — one that answers for the claim too
/// instead of promising it went (A-F1).
#[test]
fn test_the_volume_results_say_when_a_mount_takes_effect() {
    assert_eq!(create_sentence(false), "It mounts there when your pod next starts.");
    assert!(create_sentence(true).contains("can't add a volume to a live pod"), "{}", create_sentence(true));
    assert!(create_sentence(true).contains("terminate_pod then create_pod"), "{}", create_sentence(true));
    assert!(delete_sentence(true, true).contains("until its next start"), "{}", delete_sentence(true, true));
    assert_eq!(delete_sentence(false, true), "its row, every mount row and its claim are gone. Nothing mounts it now.");
    // With the claim unreachable, the copy says what will take it — the
    // startup sweep — and never that it's gone; and the mount half keeps
    // its own answer for both pod states.
    for live in [true, false] {
        let text = delete_sentence(live, false);
        assert!(text.contains("its claim is left for the startup sweep"), "{text}");
        assert!(!text.contains("claim are gone"), "an unreachable claim isn't gone: {text}");
    }
    assert!(delete_sentence(false, false).contains("Nothing mounts it now."), "{}", delete_sentence(false, false));
    assert!(delete_sentence(true, false).contains("until its next start"), "{}", delete_sentence(true, false));
}
