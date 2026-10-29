use super::*;

/// SME-51 B5: deleting a conversation removes every pod labelled with
/// it, including one whose record is already gone. That's what a
/// `create_pod` racing the delete leaves: its row cascades away with
/// the conversation, and a teardown that listed rows missed the pod.
#[tokio::test]
async fn test_teardown_deletes_a_conversations_pod_that_has_no_record() {
    let client = test_client().await;
    let pods = pods_api(&client);
    let conversation_id = unused_conversation_id();
    // Only the label matters, so the pod never needs to start: an image
    // that doesn't exist keeps it Pending and costs the cluster nothing.
    let name = format!("sandbox-{conversation_id}");
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": name, "labels": {CONVERSATION_LABEL: conversation_id.to_string(), INSTANCE_LABEL: TEST_INSTANCE}},
        "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &pod).await.expect("create pod");

    // A language server pod of the same conversation (SME-35), under
    // its own label.
    let server = format!("lsp-{conversation_id}-x");
    let server_pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": server, "labels": {crate::lsp::pods::LSP_OF_LABEL: conversation_id.to_string(), INSTANCE_LABEL: TEST_INSTANCE}},
        "spec": {"containers": [{"name": "server", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &server_pod).await.expect("create server pod");

    teardown_conversation_with(&client, conversation_id, &[], &test_instance()).await;
    let gone = tokio::time::timeout(Duration::from_secs(60), async {
        while pods.get_opt(&name).await.ok().flatten().is_some()
            || pods.get_opt(&server).await.ok().flatten().is_some()
        {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .is_ok();

    pods.delete(&name, &immediate_delete_params()).await.ok();
    pods.delete(&server, &immediate_delete_params()).await.ok();
    assert!(gone, "{name} or {server} survived its conversation's teardown");
}

/// SME-88: a pod without the conversation label (one from before
/// SME-33) is still deleted, found by its record's id.
#[tokio::test]
async fn test_teardown_deletes_our_pod_named_by_its_record_but_not_an_unlabelled_one() {
    let client = test_client().await;
    let pods = pods_api(&client);
    let conversation_id = unused_conversation_id();
    // Ours, with no conversation label (SME-88), and one from before
    // SME-115, with no label at all.
    let ours = pod_name(conversation_id + 1);
    let unlabelled = pod_name(conversation_id + 2);
    for (name, labels) in [
        (&ours, serde_json::json!({INSTANCE_LABEL: TEST_INSTANCE})),
        (&unlabelled, serde_json::json!({})),
    ] {
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"name": name, "labels": labels},
            "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
        }))
        .expect("pod");
        pods.create(&PostParams::default(), &pod).await.expect("create pod");
    }

    teardown_conversation_with(&client, conversation_id, &[conversation_id + 1, conversation_id + 2], &test_instance())
        .await;
    let ours_gone = tokio::time::timeout(Duration::from_secs(60), async {
        while pods.get_opt(&ours).await.ok().flatten().is_some() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .is_ok();
    let unlabelled_kept = pods
        .get_opt(&unlabelled)
        .await
        .expect("read the pod")
        .is_some_and(|pod| pod.metadata.deletion_timestamp.is_none());

    pods.delete(&ours, &immediate_delete_params()).await.ok();
    pods.delete(&unlabelled, &immediate_delete_params()).await.ok();
    assert!(ours_gone, "{ours} survived its conversation's teardown");
    assert!(unlabelled_kept, "teardown deleted {unlabelled}, which has no instance label");
}

/// SME-115: a server deleting its conversation N leaves another database's
/// conversation N alone: its sandbox pod, its language server pod (found
/// by label), a pod its records happen to name, and its claims. Before
/// the fix teardown deleted them all.
#[tokio::test]
async fn test_teardown_leaves_another_databases_conversation_of_the_same_id() {
    let client = test_client().await;
    let pods = pods_api(&client);
    let pvcs = pvc_api(&client);
    let theirs = "another-smelt-database";
    let conversation_id = unused_conversation_id();
    let sandbox = pod_name(conversation_id);
    let server = format!("lsp-{conversation_id}-x");
    for (name, label) in [(&sandbox, CONVERSATION_LABEL), (&server, crate::lsp::pods::LSP_OF_LABEL)] {
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"name": name, "labels": {label: conversation_id.to_string(), INSTANCE_LABEL: theirs}},
            "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
        }))
        .expect("pod");
        pods.create(&PostParams::default(), &pod).await.expect("create pod");
    }
    let claims = [docker_pvc_name(conversation_id), workspace_pvc_name(conversation_id)];
    for claim in &claims {
        make_claim(&client, claim, conversation_id, Some(theirs)).await;
    }

    teardown_conversation_with(&client, conversation_id, &[conversation_id], &test_instance()).await;
    let mut deleted = Vec::new();
    for name in [&sandbox, &server] {
        if !pods.get_opt(name).await.expect("read").is_some_and(|p| p.metadata.deletion_timestamp.is_none()) {
            deleted.push(name.clone());
        }
    }
    for claim in &claims {
        if !claim_kept(&client, claim).await {
            deleted.push(claim.clone());
        }
    }

    for name in [&sandbox, &server] {
        pods.delete(name, &immediate_delete_params()).await.ok();
    }
    for claim in &claims {
        pvcs.delete(claim, &DeleteParams::default()).await.ok();
    }
    assert!(deleted.is_empty(), "teardown deleted another database's {deleted:?}");
}

/// SME-115: a conversation's claims that are another database's (or from
/// before the fix, unadopted) are refused, never mounted, and left as
/// they were. Before the fix the start went ahead and mounted them.
#[tokio::test]
async fn test_another_databases_claims_are_never_mounted() {
    let client = test_client().await;
    let foreign = unused_conversation_id();
    let unlabelled = foreign + 1;
    make_claim(&client, &workspace_pvc_name(foreign), foreign, Some("another-smelt-database")).await;
    make_claim(&client, &workspace_pvc_name(unlabelled), unlabelled, None).await;

    let refused_foreign = ensure_conversation_pvcs(&client, foreign, &test_instance()).await;
    let refused_unlabelled = ensure_conversation_pvcs(&client, unlabelled, &test_instance()).await;
    let kept = [
        claim_kept(&client, &workspace_pvc_name(foreign)).await,
        claim_kept(&client, &workspace_pvc_name(unlabelled)).await,
    ];

    for id in [foreign, unlabelled] {
        for name in [docker_pvc_name(id), workspace_pvc_name(id)] {
            pvc_api(&client).delete(&name, &DeleteParams::default()).await.ok();
        }
    }
    match refused_foreign {
        Err(SandboxError::NotOurs { name, ownership: Ownership::Foreign }) => {
            assert_eq!(name, workspace_pvc_name(foreign))
        }
        other => panic!("another database's /workspace wasn't refused: {other:?}"),
    }
    match refused_unlabelled {
        Err(e @ SandboxError::NotOurs { ownership: Ownership::Unlabelled, .. }) => {
            assert!(e.to_string().contains("SME-115"), "{e}")
        }
        other => panic!("an unadopted /workspace wasn't refused: {other:?}"),
    }
    assert_eq!(kept, [true, true], "a refused claim was changed");
}

/// SME-115: a pod named like ours that is another database's is refused,
/// never reused (its agent runs commands for whoever connects), and left.
#[tokio::test]
async fn test_another_databases_pod_of_the_same_name_is_never_reused() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let pods = pods_api(&client);
    let session_id = unique_session_id("foreign");
    let name = format!("sandbox-{session_id}");
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": name, "labels": {INSTANCE_LABEL: "another-smelt-database"}},
        "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &pod).await.expect("create pod");

    let result = manager
        .create_with_running_timeout(
            &session_id,
            "128Mi",
            &DockerSidecar { memory: "512Mi".to_string(), storage: PodStorage::Ephemeral },
            &[],
            &[],
            Duration::from_secs(5),
            &test_instance(),
        )
        .await;
    let kept = pods.get_opt(&name).await.expect("read").is_some_and(|p| p.metadata.deletion_timestamp.is_none());

    pods.delete(&name, &immediate_delete_params()).await.ok();
    assert!(
        matches!(result, Err(SandboxError::NotOurs { ownership: Ownership::Foreign, .. })),
        "another database's pod was used: {:?}",
        result.as_ref().map(|s| &s.pod_name)
    );
    assert!(kept, "another database's pod was deleted");
}

/// SME-115: another database's volume claim of the same name is refused,
/// never mounted, and deleting our volume of that id leaves it.
#[tokio::test]
async fn test_another_databases_volume_claim_is_refused_and_left() {
    let client = test_client().await;
    let pvcs = pvc_api(&client);
    let id = unused_conversation_id();
    let name = sandbox_volume_pvc_name(id);
    pvcs.create(&PostParams::default(), &build_volume_pvc_spec(id, "another-smelt-database"))
        .await
        .expect("create the claim");
    let volume = db::SandboxVolume {
        id,
        name: "sme-115".to_string(),
        mount_path: "/data".to_string(),
        created_at: chrono::Utc::now().naive_utc(),
        updated_at: chrono::Utc::now().naive_utc(),
    };

    let mounted = ensure_volume_claims(&client, std::slice::from_ref(&volume), &test_instance()).await;
    let deleted = delete_volume_claim(&client, id, &test_instance()).await;
    let kept = claim_kept(&client, &name).await;

    pvcs.delete(&name, &DeleteParams::default()).await.ok();
    assert!(
        matches!(mounted, Err(SandboxError::NotOurs { ownership: Ownership::Foreign, .. })),
        "another database's volume claim was used: {mounted:?}"
    );
    assert!(deleted.is_ok(), "{deleted:?}");
    assert!(kept, "deleting our volume deleted another database's claim");
}

/// SME-115: what reads as this database's pod. One from before the fix
/// counts only for the database that owns those (adoption may have missed
/// it); another database's never does.
#[test]
fn test_counts_as_ours_only_ours_and_unadopted_ones_for_their_owner() {
    let owner = db::SmeltInstance { id: "ours".to_string(), owns_unlabelled: true };
    let scratch = db::SmeltInstance { id: "ours".to_string(), owns_unlabelled: false };
    for (meta, for_owner, for_scratch) in [
        (meta_with_instance(Some("ours")), true, true),
        (meta_with_instance(None), true, false),
        (meta_with_instance(Some("theirs")), false, false),
    ] {
        assert_eq!(counts_as_ours(&meta, &owner), for_owner, "{:?}", meta.labels);
        assert_eq!(counts_as_ours(&meta, &scratch), for_scratch, "{:?}", meta.labels);
    }
}

/// SME-115: the pod watch is limited to this database's pods, so another
/// smelt's Docker restarts, language server stops and finished pods never
/// reach this database's records by their ids.
#[test]
fn test_the_pod_watch_sees_only_our_instance() {
    assert_eq!(watcher_config("ours").label_selector.as_deref(), Some("smelt/instance=ours"));
}

/// SME-115: a live record whose pod name is taken by another database's
/// pod (still running) is closed as gone, and that pod is left. Before the
/// fix the record stayed open on the other pod's status, and terminating
/// it deleted that pod.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_record_whose_pod_is_another_databases_is_closed_and_the_pod_left(pool: PgPool) {
    let client = test_client().await;
    let pods = pods_api(&client);
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let row = db::create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    let name = pod_name(row.id);
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": name, "labels": {INSTANCE_LABEL: "another-smelt-database"}},
        "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &pod).await.expect("create pod");

    close_if_gone_with(&pool, &client, row.id).await;
    let closed = !db::sandbox_pod_is_live(&pool, row.id).await.expect("live?");
    let kept = pods.get_opt(&name).await.expect("read").is_some_and(|p| p.metadata.deletion_timestamp.is_none());

    pods.delete(&name, &immediate_delete_params()).await.ok();
    assert!(closed, "the record stayed open on another database's pod");
    assert!(kept, "closing the record deleted another database's pod");
}

/// SME-115 review 2: the adoption patch holds the object to its identity
/// (its uid: the API server refuses a patch carrying another uid, 422),
/// not its whole version: a new pod's status changes several times a
/// second, and a `resourceVersion` precondition failed both tries and
/// left it unadopted.
#[test]
fn test_the_adoption_patch_is_held_to_the_objects_uid_not_its_version() {
    let meta = ObjectMeta {
        name: Some("sandbox-401".to_string()),
        uid: Some("uid-401".to_string()),
        resource_version: Some("12345".to_string()),
        ..Default::default()
    };
    let patch = adoption_patch(&meta, "ours");
    assert_eq!(patch["metadata"]["labels"][INSTANCE_LABEL], "ours");
    assert_eq!(patch["metadata"]["uid"], "uid-401");
    assert!(patch["metadata"].get("resourceVersion").is_none(), "{patch}");
}

/// SME-180 review 1: deleting a `Sandbox` (a start whose git install
/// failed, or one stopped while starting) and its drop queue delete only
/// the pod it made, by uid. Once that pod is gone, another database's pod
/// can take the name, and is left: before, both deleted by name.
///
/// Harmless if killed: unique names; the other pod is labelled with
/// another instance and never starts (an image that doesn't exist).
#[tokio::test]
async fn test_a_sandboxs_delete_leaves_another_databases_pod_that_took_its_name() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let pods = pods_api(&client);
    let sandbox = manager.create(&unique_session_id("replaced"), "256Mi", &[]).await.expect("create pod");
    let name = sandbox.pod_name.clone();
    let uid = sandbox.uid.clone();

    pods.delete(&name, &immediate_delete_params()).await.expect("delete our pod");
    let gone = tokio::time::timeout(Duration::from_secs(60), async {
        while pods.get_opt(&name).await.ok().flatten().is_some() {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    })
    .await;
    let theirs: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": name, "labels": {INSTANCE_LABEL: "another-smelt-database"}},
        "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    let made = match gone {
        Ok(()) => pods.create(&PostParams::default(), &theirs).await.map(|_| ()).map_err(|e| e.to_string()),
        Err(_) => Err("our pod wasn't gone after 60s".to_string()),
    };
    let kept = |pod: Option<Pod>| pod.is_some_and(|p| p.metadata.deletion_timestamp.is_none());

    let deleted = manager.delete(sandbox).await;
    let kept_by_delete = kept(pods.get_opt(&name).await.ok().flatten());
    if !kept_by_delete {
        // Put it back, so the drop queue's check stands on its own.
        let _ = tokio::time::timeout(Duration::from_secs(60), async {
            while pods.get_opt(&name).await.ok().flatten().is_some() {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
        .await;
        pods.create(&PostParams::default(), &theirs).await.ok();
    }
    let (tx, rx) = mpsc::unbounded_channel();
    tx.send((name.clone(), uid.clone())).expect("queue");
    drop(tx);
    let drained = tokio::time::timeout(Duration::from_secs(60), drain_cleanup_queue(client.clone(), rx)).await;
    let kept_by_queue = kept(pods.get_opt(&name).await.ok().flatten());

    pods.delete(&name, &immediate_delete_params()).await.ok();
    made.expect("make the other database's pod");
    assert!(uid.is_some(), "the sandbox kept no uid");
    assert!(deleted.is_ok(), "{deleted:?}");
    assert!(drained.is_ok(), "the drop queue took over 60s");
    assert!(
        kept_by_delete && kept_by_queue,
        "another database's pod was deleted: by the sandbox's delete {}, by its drop queue {}",
        !kept_by_delete,
        !kept_by_queue
    );
}
