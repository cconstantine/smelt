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

/// How long a claim is waited out after the pods that mounted it are deleted:
/// `pvc-protection` holds it until they're really gone, which under the load
/// of the other tests' pods starting alongside is a few seconds, not one.
const CLAIM_RELEASE_WAIT: Duration = Duration::from_secs(60);
