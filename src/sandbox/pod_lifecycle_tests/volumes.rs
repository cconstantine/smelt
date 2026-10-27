//! Volumes the model creates (SME-239), against a real cluster: the claim a
//! tool makes is mounted in a real pod and survives its restart, a parent and
//! a child share one claim both ways, deleting the creating conversation
//! takes the volume with it, and the startup sweep takes a claim whose creator
//! is gone. Each scenario is `own_sandbox` + `run_then_tear_down`, so its pods
//! and claims are its database's and are deleted whether it passes or fails.

use super::*;

/// A conversation's volume, made by the tool rather than by hand: the row, its
/// creator's mount row and the claim, as `create_volume` writes them.
async fn model_volume(pool: &PgPool, conversation_id: i64, name: &str, mount_path: &str) -> db::AgentVolume {
    crate::agents::create_volume(pool, conversation_id, &serde_json::json!({"name": name, "mount_path": mount_path}))
        .await
        .expect("create_volume");
    db::agent_volume_held_by(pool, conversation_id, name).await.expect("held").expect("the volume")
}

/// SME-239: deleting the conversation that created a volume takes the volume
/// with it — its rows, both conversations' pods, and the claim those pods
/// mounted. Which conversation merely *held* the volume changes nothing here:
/// the db tests cover the mount-row half, and this one is about the objects in
/// the cluster, which `pvc-protection` keeps until the pods that mount it are
/// gone. Bounded at twice the usual scenario limit: two pods start, and the
/// claim's release is waited out rather than raced.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_deleting_the_creating_conversation_takes_the_volume_with_it(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT * 2, async {
        let parent = db::create_conversation(&pool).await.expect("parent").id;
        let volume = model_volume(&pool, parent, "shared", "/data/shared").await;

        // The child gets it through the same transaction `start_agent` runs,
        // and each conversation runs a pod mounting the one claim.
        let new = db::NewChild {
            caller: parent,
            brief: "work",
            persona_name: None,
            model: None,
            repo: None,
            max_depth: 5,
            volumes: &["shared"],
        };
        let db::CreateChild::Created { child, .. } = db::create_child(&pool, &new).await.expect("create_child") else {
            panic!("the parent holds the volume it asked to pass");
        };
        for conversation_id in [parent, child] {
            create_pod(&pool, conversation_id, PodLimitOverrides::default()).await.expect("create_pod");
        }

        let pvcs = pvc_api(&client);
        let claim = agent_volume_pvc_name(volume.id);
        assert!(pvcs.get_opt(&claim).await.expect("the claim").is_some(), "both pods mount this claim");

        crate::agents::delete_subtree(&pool, parent).await.expect("delete_subtree");

        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agent_volumes").fetch_one(&pool).await.expect("count"),
            0,
            "the volume row goes with the conversation that created it"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM conversation_volume_mounts")
                .fetch_one(&pool)
                .await
                .expect("count"),
            0,
            "and every conversation's mount row with it"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sandbox_pods").fetch_one(&pool).await.expect("count"),
            0,
            "teardown took both conversations' pods"
        );

        let claim_gone = tokio::time::timeout(CLAIM_RELEASE_WAIT, async {
            while pvcs.get_opt(&claim).await.ok().flatten().is_some() {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
        .await;
        assert!(claim_gone.is_ok(), "the claim {claim} outlived the subtree that made it");
    })
    .await;
}

/// SME-239: a volume the model made rides in its conversation's real pod,
/// and what was written there outlives the pod. The claim behind it carries
/// both labels: `smelt/instance` so no other deployment mounts it (SME-115)
/// and `smelt/agent-volume-owner` so the sweep knows whose it is. Bounded at
/// twice the scenario limit: the restart is a second pod start.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_model_volume_mounts_in_its_pod_and_survives_a_restart(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT * 2, async {
        let parent = db::create_conversation(&pool).await.expect("parent").id;
        let volume = model_volume(&pool, parent, "work", "/data/work").await;

        let pod = create_pod(&pool, parent, PodLimitOverrides::default()).await.expect("create_pod");
        let write = exec_with(
            &client,
            &pod_name(pod),
            "sandbox",
            &["sh", "-c", "echo hello > /data/work/marker.txt"],
            None,
        )
        .await
        .expect("write into the mount");
        assert_eq!(write.exit_code, 0, "the volume is mounted and writable");

        let instance = db::smelt_instance(&pool).await.expect("instance");
        let claim = pvc_api(&client).get(&agent_volume_pvc_name(volume.id)).await.expect("the claim");
        let labels = claim.metadata.labels.unwrap_or_default();
        assert_eq!(
            labels.get(INSTANCE_LABEL).map(String::as_str),
            Some(instance.id.as_str()),
            "the claim carries this deployment's instance label (SME-115)"
        );
        assert_eq!(
            labels.get(AGENT_VOLUME_LABEL).map(String::as_str),
            Some(parent.to_string().as_str()),
            "and names its creator for the sweep"
        );

        terminate_pod(&pool, parent).await.expect("terminate_pod");
        let pod = create_pod(&pool, parent, PodLimitOverrides::default()).await.expect("create_pod after the restart");
        let read = exec_with(&client, &pod_name(pod), "sandbox", &["cat", "/data/work/marker.txt"], None)
            .await
            .expect("read after the restart");
        assert_eq!(read.stdout.trim(), "hello", "the volume outlived the pod that wrote it");
    })
    .await;
}

/// SME-239's Done-when check, on the real cluster: a parent and the child it
/// passed the volume to share one claim, both ways — what one pod writes the
/// other reads — and the child's own write is still there when its pod comes
/// back. (The *refusal* half of the pass — a name the parent doesn't hold —
/// is carried by the tool test C5 without a cluster; there's no cluster
/// behaviour there to duplicate.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_parent_and_child_share_a_volume_both_ways_across_restarts(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT * 3, async {
        let parent = db::create_conversation(&pool).await.expect("parent").id;
        model_volume(&pool, parent, "shared", "/data/shared").await;
        let new = db::NewChild {
            caller: parent,
            brief: "work",
            persona_name: None,
            model: None,
            repo: None,
            max_depth: 5,
            volumes: &["shared"],
        };
        let db::CreateChild::Created { child, .. } = db::create_child(&pool, &new).await.expect("create_child") else {
            panic!("the parent holds the volume it asked to pass");
        };

        let parent_pod = create_pod(&pool, parent, PodLimitOverrides::default()).await.expect("parent pod");
        let child_pod = create_pod(&pool, child, PodLimitOverrides::default()).await.expect("child pod");

        let from_parent = "echo from parent > /data/shared/from-parent";
        exec_with(&client, &pod_name(parent_pod), "sandbox", &["sh", "-c", from_parent], None)
            .await
            .expect("parent write");
        let read = exec_with(&client, &pod_name(child_pod), "sandbox", &["cat", "/data/shared/from-parent"], None)
            .await
            .expect("child read");
        assert_eq!(read.stdout.trim(), "from parent", "the child's pod mounts the same volume");

        let from_child = "echo from child > /data/shared/from-child";
        exec_with(&client, &pod_name(child_pod), "sandbox", &["sh", "-c", from_child], None)
            .await
            .expect("child write");
        let read = exec_with(&client, &pod_name(parent_pod), "sandbox", &["cat", "/data/shared/from-child"], None)
            .await
            .expect("parent read");
        assert_eq!(read.stdout.trim(), "from child", "and the parent's pod sees the child's write");

        // The child's pod ends and comes back; its own write is still there.
        terminate_pod(&pool, child).await.expect("terminate_pod");
        let child_pod = create_pod(&pool, child, PodLimitOverrides::default()).await.expect("child pod again");
        let read = exec_with(&client, &pod_name(child_pod), "sandbox", &["cat", "/data/shared/from-child"], None)
            .await
            .expect("child read after its restart");
        assert_eq!(read.stdout.trim(), "from child", "the volume outlived the child's pod too");
    })
    .await;
}

/// SME-239: the startup sweep deletes the claim of a conversation this
/// database no longer has — the leftover of a delete cut off between the
/// rows and the claim — and keeps a live creator's claim and another
/// deployment's of the same name shape. Claims are hand-built with the
/// manager's own builder: the sweep reads labels, not rows, and this test
/// is about the objects, not how they came to be.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_agent_volume_sweep_deletes_a_claim_whose_owner_is_gone(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let instance = db::smelt_instance(&pool).await.expect("instance");
        let pvcs = pvc_api(&client);
        let gone = db::create_conversation(&pool).await.expect("gone").id;
        let live = db::create_conversation(&pool).await.expect("live").id;

        let orphan_claim = agent_volume_pvc_name(gone);
        let live_claim = agent_volume_pvc_name(live);
        // A name shape another deployment would use, labelled to a different
        // instance: nobody else's sweep, and not ours either.
        let foreign_claim = agent_volume_pvc_name(9_000_000_007);
        pvcs.create(&PostParams::default(), &build_agent_volume_pvc_spec(gone, gone, &instance.id))
            .await
            .expect("orphaned claim");
        pvcs.create(&PostParams::default(), &build_agent_volume_pvc_spec(live, live, &instance.id))
            .await
            .expect("live claim");
        pvcs.create(&PostParams::default(), &build_agent_volume_pvc_spec(9_000_000_007, gone, "not-this-deployment"))
            .await
            .expect("foreign claim");

        sqlx::query("DELETE FROM conversations WHERE id = $1").bind(gone).execute(&pool).await.expect("delete the creator");
        sweep_orphaned_agent_volume_claims_with(&client, &pool).await;

        let swept = tokio::time::timeout(CLAIM_RELEASE_WAIT, async {
            while pvcs.get_opt(&orphan_claim).await.ok().flatten().is_some() {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
        .await;
        assert!(swept.is_ok(), "the sweep left the orphaned claim {orphan_claim}");
        assert!(pvcs.get_opt(&live_claim).await.expect("live claim").is_some(), "a live creator's claim is kept");
        assert!(pvcs.get_opt(&foreign_claim).await.expect("foreign claim").is_some(), "another deployment's claim is nobody else's to sweep");

        // This test's own cleanup: teardown only deletes claims labelled with
        // this database's instance, and this one deliberately isn't.
        pvcs.delete(&foreign_claim, &DeleteParams::default()).await.ok();
    })
    .await;
}

/// SME-239 (the design's SME-115 line, proved for model volumes): a claim
/// already there under the name, labelled to another deployment, refuses the
/// pod start rather than mounting a foreign database's data. Rows only, no
/// tool: the whole point is a pod start meeting a claim it didn't make.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_foreign_agent_volume_claim_refuses_the_pod_start(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("conversation").id;
        let volume_id: i64 =
            sqlx::query_scalar("INSERT INTO agent_volumes (owner_conversation_id, name, mount_path) VALUES ($1, 'shared', '/data/shared') RETURNING id")
                .bind(conversation)
                .fetch_one(&pool)
                .await
                .expect("volume row");
        sqlx::query("INSERT INTO conversation_volume_mounts (conversation_id, volume_id) VALUES ($1, $2)")
            .bind(conversation)
            .bind(volume_id)
            .execute(&pool)
            .await
            .expect("mount row");

        let claim = agent_volume_pvc_name(volume_id);
        pvc_api(&client)
            .create(&PostParams::default(), &build_agent_volume_pvc_spec(volume_id, conversation, "not-this-deployment"))
            .await
            .expect("foreign claim");

        let refused = create_pod(&pool, conversation, PodLimitOverrides::default()).await;
        assert!(
            matches!(&refused, Err(SandboxError::NotOurs { name, .. }) if name == &claim),
            "the start should refuse a claim it didn't make, got {refused:?}"
        );

        // As in the sweep test: teardown won't delete a claim not labelled
        // with this database's instance.
        pvc_api(&client).delete(&claim, &DeleteParams::default()).await.ok();
    })
    .await;
}

/// How long a claim is waited out after the pods that mounted it are deleted:
/// `pvc-protection` holds it until they're really gone, which under the load
/// of the other tests' pods starting alongside is a few seconds, not one.
const CLAIM_RELEASE_WAIT: Duration = Duration::from_secs(60);
