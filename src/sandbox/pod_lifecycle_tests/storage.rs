use super::*;

/// SME-253: a claim already being deleted is never mounted by the next
/// pod. A holder pod (one that never starts, so nothing real is mounted)
/// keeps a deleted claim alive with a `deletion_timestamp` — the doomed
/// volume would fall out from under any pod started onto it. Starting a
/// pod over it waits the claim out and creates a fresh one; the holder is
/// freed midway so the wait ends, as it does in the real race.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_terminating_claim_is_not_reused_by_the_next_pod(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let instance = db::smelt_instance(&pool).await.expect("instance");
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let id = conversation.id;

        ensure_conversation_pvcs(&client, id, &instance).await.expect("claims");
        let name = workspace_pvc_name(id);
        let doomed = pvc_api(&client).get(&name).await.expect("get the workspace claim");
        let doomed_uid = doomed.metadata.uid.clone().expect("a claim's uid");

        // A pod that never starts (an image that doesn't exist) mounting
        // the claim: deleting the claim then leaves it readable, with a
        // deletion timestamp, until this pod goes (pvc-protection).
        let holder: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {
                "name": format!("doomed-claim-holder-{id}"),
                "labels": {CONVERSATION_LABEL: id.to_string(), INSTANCE_LABEL: instance.id.clone()},
            },
            "spec": {
                "restartPolicy": "Never",
                "containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}],
                "volumes": [{"name": "workspace", "persistentVolumeClaim": {"claimName": name}}],
            },
        }))
        .expect("pod");
        let holder_name = pods_api(&client)
            .create(&PostParams::default(), &holder)
            .await
            .expect("create the holder pod")
            .metadata
            .name
            .expect("a created pod's name");

        pvc_api(&client).delete(&name, &DeleteParams::default()).await.expect("delete the claim");
        let doomed_now = pvc_api(&client).get(&name).await.expect("read the claim back");
        assert!(doomed_now.metadata.deletion_timestamp.is_some(), "the holder should leave it Terminating, not gone");

        // The holder goes a moment after the wait begins, as the real
        // removal's pod delete frees the claim mid-wait.
        let freer = client.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            pods_api(&freer).delete(&holder_name, &immediate_delete_params()).await.ok();
        });

        ensure_conversation_pvcs(&client, id, &instance)
            .await
            .expect("a Terminating claim is waited out and re-created, not reused");
        let fresh = pvc_api(&client).get(&name).await.expect("get the fresh claim");
        assert_ne!(
            fresh.metadata.uid.clone().expect("the fresh claim's uid"),
            doomed_uid,
            "the next pod gets a fresh claim, never the doomed one"
        );
        assert!(fresh.metadata.deletion_timestamp.is_none(), "the re-created claim isn't being deleted");
    })
    .await;
}
