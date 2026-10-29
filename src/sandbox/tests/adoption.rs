use super::*;

fn owner() -> db::SmeltInstance {
    db::SmeltInstance { id: "ours".to_string(), owns_unlabelled: true }
}

fn named_meta(name: &str, labels: &[(&str, &str)]) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.to_string()),
        labels: Some(labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()),
        ..Default::default()
    }
}

/// SME-115: adoption labels only what this database made before the fix:
/// never without `owns_unlabelled`, never an object already labelled
/// (another database's, or ours), and only one whose conversation or
/// volume this database still has.
#[test]
fn test_adoption_takes_only_unlabelled_objects_of_our_conversations_and_volumes() {
    let conversations = std::collections::HashSet::from([1528]);
    let volumes = std::collections::HashSet::from([3]);
    let adopts = |meta: &ObjectMeta, instance: &db::SmeltInstance| should_adopt(meta, instance, &conversations, &volumes);
    let claim = named_meta("sandbox-workspace-1528", &[(CONVERSATION_LABEL, "1528")]);
    let pod = named_meta("sandbox-401", &[(CONVERSATION_LABEL, "1528")]);
    let volume = named_meta("sandbox-volume-3", &[]);
    for meta in [&claim, &pod, &volume] {
        assert!(adopts(meta, &owner()), "{:?}", meta.name);
        assert!(!adopts(meta, &db::SmeltInstance { owns_unlabelled: false, ..owner() }), "a scratch database adopted {:?}", meta.name);
    }
    let gone = named_meta("sandbox-workspace-7", &[(CONVERSATION_LABEL, "7")]);
    let gone_volume = named_meta("sandbox-volume-4", &[]);
    let theirs = named_meta("sandbox-workspace-1528", &[(CONVERSATION_LABEL, "1528"), (INSTANCE_LABEL, "theirs")]);
    let already = named_meta("sandbox-workspace-1528", &[(CONVERSATION_LABEL, "1528"), (INSTANCE_LABEL, "ours")]);
    let stranger = named_meta("something-else", &[]);
    for meta in [&gone, &gone_volume, &theirs, &already, &stranger] {
        assert!(!adopts(meta, &owner()), "adopted {:?} {:?}", meta.name, meta.labels);
    }
}

/// SME-115: a language server pod is adopted with the sandbox pod it runs
/// next to, never otherwise.
#[test]
fn test_adoption_takes_a_server_pod_only_with_its_sandbox_pod() {
    let ours = std::collections::HashSet::from([401]);
    let server = |pod: &str, labels: &[(&str, &str)]| {
        let mut all = vec![(crate::lsp::pods::LSP_POD_LABEL, pod)];
        all.extend_from_slice(labels);
        named_meta(&format!("lsp-{pod}-x"), &all)
    };
    assert!(should_adopt_server(&server("401", &[]), &owner(), &ours));
    assert!(!should_adopt_server(&server("401", &[]), &db::SmeltInstance { owns_unlabelled: false, ..owner() }, &ours));
    assert!(!should_adopt_server(&server("402", &[]), &owner(), &ours), "its sandbox pod isn't ours");
    assert!(!should_adopt_server(&server("401", &[(INSTANCE_LABEL, "theirs")]), &owner(), &ours));
}

/// The instance label of `name`'s object, read back from the cluster.
async fn instance_of<K>(api: &Api<K>, name: &str) -> Option<String>
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    api.get(name).await.expect("read").meta().labels.as_ref().and_then(|l| l.get(INSTANCE_LABEL).cloned())
}

/// The unlabelled objects of one conversation, as a database from before
/// SME-115 left them: its claim, its pod, and a language server pod next
/// to it; plus a claim of a conversation the database no longer has.
/// Returns (claim, pod, server pod, gone claim) names.
async fn make_pre_fix_objects(client: &kube::Client, conversation_id: i64, pod_id: i64) -> [String; 4] {
    let pods = pods_api(client);
    let claim = workspace_pvc_name(conversation_id);
    let gone = workspace_pvc_name(conversation_id + 1);
    make_claim(client, &claim, conversation_id, None).await;
    make_claim(client, &gone, conversation_id + 1, None).await;
    let pod = pod_name(pod_id);
    let server = format!("lsp-{pod_id}-x");
    for (name, labels) in [
        (&pod, serde_json::json!({CONVERSATION_LABEL: conversation_id.to_string()})),
        (&server, serde_json::json!({
            crate::lsp::pods::LSP_OF_LABEL: conversation_id.to_string(),
            crate::lsp::pods::LSP_POD_LABEL: pod_id.to_string(),
        })),
    ] {
        let spec: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"name": name, "labels": labels},
            "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
        }))
        .expect("pod");
        pods.create(&PostParams::default(), &spec).await.expect("create pod");
    }
    [claim, pod, server, gone]
}

async fn remove_pre_fix_objects(client: &kube::Client, [claim, pod, server, gone]: &[String; 4]) {
    for name in [pod, server] {
        pods_api(client).delete(name, &immediate_delete_params()).await.ok();
    }
    for name in [claim, gone] {
        pvc_api(client).delete(name, &DeleteParams::default()).await.ok();
    }
}

/// SME-115: the database that made the objects from before the fix (the
/// dev database) labels its conversation's claim, pod and server pod as
/// its own at startup, so its watch, sweep and teardown see them again; a
/// claim of a conversation it no longer has is left as it was.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_owning_database_adopts_its_unlabelled_objects(pool: PgPool) {
    let client = test_client().await;
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    sqlx::query("UPDATE smelt_instance SET owns_unlabelled = true").execute(&pool).await.expect("an owning database");
    let ours = db::smelt_instance(&pool).await.expect("instance").id;
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let pod_row = db::create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    let names = make_pre_fix_objects(&client, conversation.id, pod_row.id).await;

    adopt_unlabelled_objects_with(&client, &pool).await;
    let labels = [
        instance_of(&pvc_api(&client), &names[0]).await,
        instance_of(&pods_api(&client), &names[1]).await,
        instance_of(&pods_api(&client), &names[2]).await,
        instance_of(&pvc_api(&client), &names[3]).await,
    ];

    remove_pre_fix_objects(&client, &names).await;
    assert_eq!(labels, [Some(ours.clone()), Some(ours.clone()), Some(ours), None]);
}

/// SME-115: a database that didn't make them (a scratch one) adopts
/// nothing, even for a conversation id it happens to have too.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_scratch_database_adopts_nothing(pool: PgPool) {
    let client = test_client().await;
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let pod_row = db::create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    let names = make_pre_fix_objects(&client, conversation.id, pod_row.id).await;

    adopt_unlabelled_objects_with(&client, &pool).await;
    let labels = [
        instance_of(&pvc_api(&client), &names[0]).await,
        instance_of(&pods_api(&client), &names[1]).await,
        instance_of(&pods_api(&client), &names[2]).await,
        instance_of(&pvc_api(&client), &names[3]).await,
    ];

    remove_pre_fix_objects(&client, &names).await;
    assert_eq!(labels, [None, None, None, None]);
}

/// SME-115: when startup's adoption missed a claim, the owning database
/// adopts it when a pod first needs it, instead of locking the
/// conversation out of its /workspace until a restart. A scratch database
/// still refuses it.
#[tokio::test]
async fn test_a_missed_claim_is_adopted_when_the_owner_needs_it() {
    let client = test_client().await;
    let owned = unused_conversation_id();
    let refused = owned + 1;
    let owner = db::SmeltInstance { id: TEST_INSTANCE.to_string(), owns_unlabelled: true };
    make_claim(&client, &workspace_pvc_name(owned), owned, None).await;
    make_claim(&client, &workspace_pvc_name(refused), refused, None).await;

    let adopted = ensure_conversation_pvcs(&client, owned, &owner).await;
    let label = instance_of(&pvc_api(&client), &workspace_pvc_name(owned)).await;
    let scratch = ensure_conversation_pvcs(&client, refused, &test_instance()).await;

    for id in [owned, refused] {
        for name in [docker_pvc_name(id), workspace_pvc_name(id)] {
            pvc_api(&client).delete(&name, &DeleteParams::default()).await.ok();
        }
    }
    assert!(adopted.is_ok(), "{adopted:?}");
    assert_eq!(label.as_deref(), Some(TEST_INSTANCE));
    assert!(matches!(scratch, Err(SandboxError::NotOurs { ownership: Ownership::Unlabelled, .. })), "{scratch:?}");
}

/// SME-115 review 1: when startup's adoption missed a conversation's pod
/// and claims (a transient API error), the database that owns objects
/// from before the fix still deletes them with the conversation, instead
/// of leaving them for good. A database that doesn't own them leaves them
/// (`test_teardown_deletes_our_pod_named_by_its_record_but_not_an_unlabelled_one`).
#[tokio::test]
async fn test_the_owner_tears_down_a_conversation_adoption_missed() {
    let client = test_client().await;
    let pods = pods_api(&client);
    let conversation_id = unused_conversation_id();
    let owner = db::SmeltInstance { id: TEST_INSTANCE.to_string(), owns_unlabelled: true };
    let pod = pod_name(conversation_id);
    let spec: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": pod, "labels": {CONVERSATION_LABEL: conversation_id.to_string()}},
        "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &spec).await.expect("create pod");
    let claims = [docker_pvc_name(conversation_id), workspace_pvc_name(conversation_id)];
    for claim in &claims {
        make_claim(&client, claim, conversation_id, None).await;
    }

    teardown_conversation_with(&client, conversation_id, &[], &owner).await;
    let mut kept = Vec::new();
    if pods.get_opt(&pod).await.expect("read").is_some_and(|p| p.metadata.deletion_timestamp.is_none()) {
        kept.push(pod.clone());
    }
    for claim in &claims {
        if claim_kept(&client, claim).await {
            kept.push(claim.clone());
        }
    }

    pods.delete(&pod, &immediate_delete_params()).await.ok();
    for claim in &claims {
        pvc_api(&client).delete(claim, &DeleteParams::default()).await.ok();
    }
    assert!(kept.is_empty(), "the owning database left its own conversation's {kept:?}");
}

/// SME-115 review 1: stopping a pod startup's adoption missed deletes it
/// for the database that owns it, instead of closing the record and
/// leaving the pod running (every later start of the conversation would
/// then wait on it and time out).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_owner_terminates_a_pod_adoption_missed(pool: PgPool) {
    let client = test_client().await;
    let pods = pods_api(&client);
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    sqlx::query("UPDATE smelt_instance SET owns_unlabelled = true").execute(&pool).await.expect("an owning database");
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let row = db::create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    let name = pod_name(row.id);
    let spec: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": name, "labels": {CONVERSATION_LABEL: conversation.id.to_string()}},
        "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &spec).await.expect("create pod");

    let closed = force_terminate_pod_with(&pool, &client, row.id).await;
    let kept = pods.get_opt(&name).await.expect("read").is_some_and(|p| p.metadata.deletion_timestamp.is_none());

    pods.delete(&name, &immediate_delete_params()).await.ok();
    assert!(closed.is_ok(), "{closed:?}");
    assert!(!kept, "the owning database closed the record and left its pod running");
}

/// A pod from before SME-33 as the dev database left it: no labels at all.
async fn make_bare_pod(client: &kube::Client, name: &str) {
    let spec: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": name},
        "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods_api(client).create(&PostParams::default(), &spec).await.expect("create pod");
}

/// Whether pod `name` is still there and not being deleted.
async fn pod_kept(client: &kube::Client, name: &str) -> bool {
    pods_api(client).get_opt(name).await.expect("read").is_some_and(|p| p.metadata.deletion_timestamp.is_none())
}

/// SME-115 review 2: a pod from before SME-33 has no labels at all (SME-88).
/// The database that owns such objects still stops it, tears it down with
/// its conversation, and adopts it at startup while its record is live:
/// the record names it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_owner_handles_a_pod_with_no_labels_named_by_its_record(pool: PgPool) {
    let client = test_client().await;
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    sqlx::query("UPDATE smelt_instance SET owns_unlabelled = true").execute(&pool).await.expect("an owning database");
    let owner = db::smelt_instance(&pool).await.expect("instance");
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let stopped = db::create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    make_bare_pod(&client, &pod_name(stopped.id)).await;
    let closed = force_terminate_pod_with(&pool, &client, stopped.id).await;
    let stopped_kept = pod_kept(&client, &pod_name(stopped.id)).await;

    // Torn down with its conversation, named by the record read before
    // the delete.
    let torn = db::create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    make_bare_pod(&client, &pod_name(torn.id)).await;
    teardown_conversation_with(&client, conversation.id, &[torn.id], &owner).await;
    let torn_kept = pod_kept(&client, &pod_name(torn.id)).await;

    // Adopted at startup while its record is live.
    let other = db::create_conversation(&pool).await.expect("conversation");
    let live = db::create_sandbox_pod(&pool, other.id).await.expect("pod row");
    make_bare_pod(&client, &pod_name(live.id)).await;
    adopt_unlabelled_objects_with(&client, &pool).await;
    let label = instance_of(&pods_api(&client), &pod_name(live.id)).await;

    for id in [stopped.id, torn.id, live.id] {
        pods_api(&client).delete(&pod_name(id), &immediate_delete_params()).await.ok();
    }
    assert!(closed.is_ok(), "{closed:?}");
    assert!(!stopped_kept, "stopping its record left the pod running");
    assert!(!torn_kept, "deleting its conversation left the pod");
    assert_eq!(label.as_deref(), Some(owner.id.as_str()), "startup didn't adopt a live record's pod");
}

/// SME-115 review 2: deleting a volume whose claim startup's adoption
/// missed deletes the claim for the database that owns it, instead of
/// leaving the user's data in the cluster for good once the row is gone.
#[tokio::test]
async fn test_the_owner_deletes_a_volume_claim_adoption_missed() {
    let client = test_client().await;
    let id = unused_conversation_id();
    let name = sandbox_volume_pvc_name(id);
    let mut spec = build_volume_pvc_spec(id, "unused");
    spec.metadata.labels = None;
    pvc_api(&client).create(&PostParams::default(), &spec).await.expect("create the claim");
    let owner = db::SmeltInstance { id: TEST_INSTANCE.to_string(), owns_unlabelled: true };

    let deleted = delete_volume_claim(&client, id, &owner).await;
    let kept = claim_kept(&client, &name).await;

    pvc_api(&client).delete(&name, &DeleteParams::default()).await.ok();
    assert!(deleted.is_ok(), "{deleted:?}");
    assert!(!kept, "the owning database left its own volume's claim");
}

/// SME-115: a new pod waits out the conversation's stopping pods, ours
/// and ones from before the fix, but never another database's pod
/// labelled with the same conversation.
#[tokio::test]
async fn test_the_wait_for_old_pods_ignores_another_databases() {
    let client = test_client().await;
    let pods = pods_api(&client);
    let conversation_id = unused_conversation_id();
    let foreign = pod_name(conversation_id);
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": foreign, "labels": {CONVERSATION_LABEL: conversation_id.to_string(), INSTANCE_LABEL: "another-smelt-database"}},
        "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
    }))
    .expect("pod");
    pods.create(&PostParams::default(), &pod).await.expect("create pod");
    let foreign_ignored =
        wait_for_conversation_pods_gone(&client, conversation_id, Duration::from_secs(3), TEST_INSTANCE, &[]).await;

    // The same pod with no instance label: waited on.
    let unlabel = serde_json::json!({"metadata": {"labels": {INSTANCE_LABEL: null}}});
    pods.patch(&foreign, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(&unlabel))
        .await
        .expect("drop the instance label");
    let unlabelled_waited =
        wait_for_conversation_pods_gone(&client, conversation_id, Duration::from_secs(2), TEST_INSTANCE, &[]).await;

    pods.delete(&foreign, &immediate_delete_params()).await.ok();
    assert!(foreign_ignored.is_ok(), "waited on another database's pod: {foreign_ignored:?}");
    assert!(
        matches!(unlabelled_waited, Err(SandboxError::Timeout(_))),
        "a pod from before the fix may be ours, and must be waited out: {unlabelled_waited:?}"
    );
}

/// SME-115's regression test: a server whose database has no
/// conversations (B, a scratch check server) sweeps the shared namespace
/// at startup. It must delete only its own orphan, never another
/// database's claims (A, the dev server's) or one from before the fix.
/// Before the fix it deleted all three. Runs the sweep for real in the
/// shared test namespace: it can't reach another test's claims either.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_sweep_deletes_only_its_own_databases_orphans(pool: PgPool) {
    let client = test_client().await;
    let ours = db::smelt_instance(&pool).await.expect("instance").id;
    let base = unused_conversation_id();
    let theirs = format!("sandbox-workspace-{base}");
    let before_the_fix = format!("sandbox-workspace-{}", base + 1);
    let our_orphan = format!("sandbox-workspace-{}", base + 2);
    make_claim(&client, &theirs, base, Some("another-smelt-database")).await;
    make_claim(&client, &before_the_fix, base + 1, None).await;
    make_claim(&client, &our_orphan, base + 2, Some(&ours)).await;

    sweep_orphaned_conversation_claims_with(&client, &pool).await;

    let kept_theirs = claim_kept(&client, &theirs).await;
    let kept_unlabelled = claim_kept(&client, &before_the_fix).await;
    let kept_ours = claim_kept(&client, &our_orphan).await;
    for name in [&theirs, &before_the_fix, &our_orphan] {
        let _ = pvc_api(&client).delete(name, &DeleteParams::default()).await;
    }
    assert!(kept_theirs, "the sweep deleted another database's claim");
    assert!(kept_unlabelled, "the sweep deleted a claim with no instance label");
    assert!(!kept_ours, "the sweep left its own orphan");
}
