use kube::api::{Patch, PatchParams};

use super::*;

/// Waits for `holder` to be Running and for `claim` to carry the
/// `pvc-protection` finalizer — the moment a delete of it is guaranteed
/// to leave it Terminating rather than finish (SME-253 review 1). A pod
/// only holds a claim once it's scheduled and the protection controller
/// has caught up, so deleting the moment after creating the holder is a
/// race, not a held claim. Holders ask for tiny resources explicitly:
/// the namespaces' LimitRange otherwise defaults every container to a
/// 64Gi request, which no node here can fit.
async fn wait_holder_mounting(client: &kube::Client, holder: &str, claim: &str) {
    let running = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let phase = pods_api(client).get(holder).await.expect("get holder").status.unwrap_or_default().phase;
            if phase.as_deref() == Some("Running") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(running.is_ok(), "{holder} never reached Running");
    let protected = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let finalizers = pvc_api(client).get(claim).await.expect("get claim").metadata.finalizers.unwrap_or_default();
            if finalizers.iter().any(|f| f == "kubernetes.io/pvc-protection") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(protected.is_ok(), "{claim} never gained the pvc-protection finalizer while {holder} mounted it");
}

/// Waits for `holder` to be scheduled, for a holder whose container never
/// starts (an image that doesn't exist): protection counts a pod as using
/// a claim once it has a node, Running or not, and with a claim that binds
/// on its first consumer that's only once the provisioner has made its
/// volume. Before it, a delete of the claim (or of the pod) finishes at
/// once, and a test of what holds the claim proves nothing (SME-341).
async fn wait_holder_scheduled(client: &kube::Client, holder: &str) {
    let scheduled = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let pod = pods_api(client).get(holder).await.expect("read the holder pod");
            if pod.spec.and_then(|s| s.node_name).is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    assert!(scheduled.is_ok(), "{holder} was never scheduled, so it couldn't hold its claim");
}

/// SME-253: a claim already being deleted is never mounted by the next
/// pod. A scheduled holder pod (its container never starts, so nothing
/// runs on the volume) keeps a deleted claim alive with a `deletion_timestamp` — the doomed
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
                // A request of its own: given none, the namespace's
                // LimitRange (max only, 64Gi) makes the default request
                // 64Gi too, which no test node has, so the holder was never
                // scheduled and never held the claim (SME-341).
                "containers": [{
                    "name": "sandbox",
                    "image": "smelt.invalid/none:0",
                    "resources": {"requests": {"memory": "16Mi"}, "limits": {"memory": "16Mi"}},
                }],
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
        // Deleted before the holder is scheduled, nothing held the claim:
        // it read Terminating only until the protection controller let it
        // go, and the read below lost that race in CI ("read the claim
        // back: ... not found", SME-341).
        wait_holder_scheduled(&client, &holder_name).await;

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

/// Bound wait: both `names`' claims read gone, and nothing says so after a minute.
async fn claims_gone(client: &kube::Client, names: &[String]) -> bool {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let mut left = 0;
            for name in names {
                if pvc_api(client).get_opt(name).await.ok().flatten().is_some() {
                    left += 1;
                }
            }
            if left == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    })
    .await
    .is_ok()
}

/// SME-253: removing a running seat's storage stops the pod first (a held
/// claim cannot go) and deletes both claims; the conversation's row stays.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_removing_storage_while_a_pod_runs_stops_it_and_deletes_both_claims(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT * 2, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let id = conversation.id;
        create_pod(&pool, id, PodLimitOverrides::default()).await.expect("create_pod");
        let terminal = create_terminal(&pool, id).await.expect("create_terminal");
        run_and_wait(&pool, id, terminal, "write-marker", "echo gone > /workspace/removal-marker").await;

        let removal = remove_conversation_storage(&pool, id).await.expect("remove_conversation_storage");
        assert!(removal.pod_stopped, "the running pod was stopped before the claims were deleted");
        assert_eq!((removal.claims_deleted, removal.claims_failed), (2, 0), "{removal:?}");
        assert!(
            db::list_sandbox_pods(&pool, id).await.expect("live pods").is_empty(),
            "the pod row was terminated"
        );
        assert!(db::conversation_exists(&pool, id).await.expect("conversation"), "the conversation itself stays");

        let names = [workspace_pvc_name(id), docker_pvc_name(id)];
        assert!(claims_gone(&client, &names).await, "both claims go: {names:?}");
        let left = pods_api(&client)
            .list(&ListParams::default().labels(&format!("{CONVERSATION_LABEL}={id}")))
            .await
            .expect("list the conversation's pods")
            .items;
        assert!(left.is_empty(), "the conversation's pods are gone");
    })
    .await;
}

/// SME-253: a stopped conversation is exactly the holder worth removing —
/// its disk outlived its pods. Removal leaves no claims, and reports
/// there was no pod to stop.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_removing_a_stopped_conversations_storage_leaves_no_claims(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let id = conversation.id;
        create_pod(&pool, id, PodLimitOverrides::default()).await.expect("create_pod");
        terminate_pod(&pool, id).await.expect("terminate_pod");
        // As a quiet stop leaves it, an attempt of the grace pass's
        // recorded too (SME-354 review 1, M1).
        sqlx::query("UPDATE sandbox_pods SET quietly_stopped_at = now(), storage_removal_started_at = now() WHERE conversation_id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .expect("quiet-stop mark");
        let names = [workspace_pvc_name(id), docker_pvc_name(id)];
        // Stopping frees no disk: both claims stay until removed.
        for name in &names {
            assert!(pvc_api(&client).get_opt(name).await.expect("get_opt").is_some(), "{name} outlives the pod");
        }

        let removal = remove_conversation_storage(&pool, id).await.expect("remove_conversation_storage");
        assert!(!removal.pod_stopped, "no live pod to stop");
        assert_eq!((removal.claims_deleted, removal.claims_failed), (2, 0), "{removal:?}");
        assert!(claims_gone(&client, &names).await, "a stopped conversation's removal leaves no claims");
        let marks: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sandbox_pods WHERE conversation_id = $1
               AND (quietly_stopped_at IS NOT NULL OR storage_removal_started_at IS NOT NULL)",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("read the marks");
        assert_eq!(marks, 0, "the user's removal settles the grace pass's quiet stop");
    })
    .await;
}

/// SME-253's done-when: the next sandbox after a removal starts with
/// fresh empty volumes — and the claim is made again under its name,
/// labelled with this database.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_fresh_pod_after_storage_removal_finds_an_empty_workspace(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT * 3, async {
        let instance = db::smelt_instance(&pool).await.expect("instance");
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let id = conversation.id;
        create_pod(&pool, id, PodLimitOverrides::default()).await.expect("create_pod");
        let terminal = create_terminal(&pool, id).await.expect("create_terminal");
        run_and_wait(&pool, id, terminal, "write-marker", "echo gone > /workspace/removal-marker").await;
        remove_conversation_storage(&pool, id).await.expect("remove_conversation_storage");

        create_pod(&pool, id, PodLimitOverrides::default()).await.expect("second create_pod");
        let terminal = create_terminal(&pool, id).await.expect("second create_terminal");
        run_and_wait(&pool, id, terminal, "list-workspace", "ls -A /workspace; echo LISTED:$?").await;
        let listing = db::read_terminal_output(&pool, "list-workspace", &["stdout"], 0, 50)
            .await
            .expect("read_terminal_output")
            .iter()
            .map(|l| l.data.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(listing.contains("LISTED:0"), "ls ran: {listing}");
        assert!(!listing.contains("removal-marker"), "the fresh volume is empty: {listing}");

        let fresh = pvc_api(&client).get(&workspace_pvc_name(id)).await.expect("the claim is made again");
        assert!(fresh.metadata.deletion_timestamp.is_none(), "the re-made claim isn't doomed");
        assert_eq!(
            fresh.metadata.labels.unwrap_or_default().get(INSTANCE_LABEL).cloned(),
            Some(instance.id),
            "the fresh claim is labelled with this database"
        );
    })
    .await;
}

/// SME-115 on the removal path: claims are deleted by name and only ours
/// — another database's pair of the very same names (its conversation of
/// the same id) is left untouched, and the removal says it deleted
/// nothing rather than failing.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_removal_leaves_another_databases_claim_of_the_same_names_alone(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let id = conversation.id;
        let mut theirs = Vec::new();
        for name in [docker_pvc_name(id), workspace_pvc_name(id)] {
            let spec = build_conversation_pvc_spec(name.clone(), id, "1Mi".to_string(), "other-smelt-instance");
            let made = pvc_api(&client).create(&PostParams::default(), &spec).await.expect("create their claim");
            theirs.push((name, made.metadata.uid.expect("their claim's uid")));
        }

        let removal = remove_conversation_storage(&pool, id).await.expect("a removal over theirs leaves, doesn't delete");
        assert_eq!((removal.claims_deleted, removal.claims_failed), (0, 0), "{removal:?}");
        for (name, uid) in &theirs {
            let claim = pvc_api(&client).get(name).await.expect("their claim is left");
            assert_eq!(claim.metadata.uid.expect("uid"), *uid, "{name} was deleted or replaced");
            assert!(claim.metadata.deletion_timestamp.is_none(), "{name} wasn't even asked to delete");
        }
        // Our own way out: the harness's teardown deletes what's labelled
        // with this database, which these aren't — left behind they'd hold
        // these names from every later test for an hour, until the sweep.
        for (name, _) in theirs {
            pvc_api(&client).delete(&name, &DeleteParams::default()).await.expect("clean up their claim");
        }
    })
    .await;
}

/// SME-253: a language-server pod mounts the workspace claim too, so a
/// removal deletes it first (its lazy restart already exists for
/// teardown) and waits for it. Hand-made, not a real LSP pod: the label
/// wiring is what's tested; a hands-on check covers a real one.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_removal_stops_the_conversations_language_server_pods_and_frees_the_claim(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let instance = db::smelt_instance(&pool).await.expect("instance");
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let id = conversation.id;
        ensure_conversation_pvcs(&client, id, &instance).await.expect("claims");
        let name = workspace_pvc_name(id);
        let holder: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {
                "name": format!("lsp-holder-{id}"),
                "labels": {crate::lsp::pods::LSP_OF_LABEL: id.to_string(), INSTANCE_LABEL: instance.id.clone()},
            },
            "spec": {
                "restartPolicy": "Never",
                // Running, and slow to go: it takes SIGTERM 5 s before it
                // exits, so a removal that didn't wait for it would return
                // while it's still there (SME-341 review), and one that does
                // waits about 5 s, well inside its 30 s. A request of its
                // own, as the namespace's LimitRange would otherwise make it
                // 64Gi.
                "containers": [{
                    "name": "sleep",
                    "image": default_sandbox_image(),
                    "imagePullPolicy": default_image_pull_policy(),
                    "command": ["sh", "-c", "trap 'sleep 5; exit 0' TERM; sleep 3600 & wait"],
                    "resources": {"requests": {"memory": "32Mi", "cpu": "10m"}, "limits": {"memory": "256Mi"}},
                }],
                "volumes": [{"name": "workspace", "persistentVolumeClaim": {"claimName": name}}],
            },
        }))
        .expect("pod");
        pods_api(&client).create(&PostParams::default(), &holder).await.expect("create the LSP holder");
        // Mounting first, so the pod holds the claim; one not yet there is
        // deleted at once, waited for or not (SME-341 review).
        let holder_name = format!("lsp-holder-{id}");
        wait_holder_mounting(&client, &holder_name, &name).await;

        let removal = remove_conversation_storage(&pool, id).await.expect("remove_conversation_storage");
        assert_eq!(removal.lsp_pods_stopped, 1, "the conversation's language-server pod was deleted");
        // Removal waits for the pod to be gone before it goes on: by the
        // time it returns, the pod is. A removal that only asked for the
        // delete returns while the pod is still terminating.
        assert!(
            pods_api(&client).get_opt(&holder_name).await.expect("look for the LSP holder").is_none(),
            "removal returned before the language-server pod mounting the claim was gone"
        );
        assert!(!removal.pod_stopped, "no sandbox pod row existed");

        let gone = tokio::time::timeout(Duration::from_secs(60), async {
            while pvc_api(&client).get_opt(&name).await.ok().flatten().is_some() {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
        .await;
        assert!(gone.is_ok(), "the claim the LSP pod mounted is freed after removal");
    })
    .await;
}

/// SME-253 review 1, finding 1: a start racing a removal can never mount a
/// doomed claim — the removal takes the conversation's pod-start lock, so a
/// start is either wholly before it or waits for storage that is already
/// gone. The test holds that lock the way a start would; removal must wait.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_removal_waits_for_the_pod_start_lock(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT * 2, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let id = conversation.id;
        create_pod(&pool, id, PodLimitOverrides::default()).await.expect("create_pod");
        terminate_pod(&pool, id).await.expect("terminate_pod");
        let names = [workspace_pvc_name(id), docker_pvc_name(id)];

        let start_lock = pod_start_lock(id);
        let start_held = start_lock.lock().await;
        let removal = tokio::time::timeout(Duration::from_secs(5), remove_conversation_storage(&pool, id)).await;
        assert!(removal.is_err(), "a removal waits for the pod-start lock a start holds");
        for name in &names {
            assert!(pvc_api(&client).get_opt(name).await.expect("get_opt").is_some(), "{name} is untouched while the start lock is held");
        }
        drop(start_held);

        // The timed-out request's removal was never cut off (SME-354 B4):
        // it took the lock as soon as it freed, so the claims go without
        // another call. One made now queues behind it and finds them gone.
        assert!(claims_gone(&client, &names).await, "both claims go once the lock frees");
        let removal = remove_conversation_storage(&pool, id).await.expect("remove after the start released");
        assert_eq!((removal.claims_deleted, removal.claims_failed), (0, 0), "the first removal took them: {removal:?}");
    })
    .await;
}

/// SME-354 B4: a removal whose request goes away midway — a closed tab
/// drops the server function's future — still finishes. The request is
/// dropped the moment the pod's row reads closed, while the removal waits
/// out the pod with both claims still standing; the claims then go and
/// the conversation's repos are marked, as a whole removal does.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_removal_whose_request_is_dropped_midway_still_finishes(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT * 2, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let id = conversation.id;
        let pod = create_pod(&pool, id, PodLimitOverrides::default()).await.expect("create_pod");
        let repo = db::create_conversation_repo(&pool, id, "u", &format!("k-{id}"), None, "r").await.expect("repo row");
        db::set_repo_ready(&pool, repo.id, 1, "main", None, &[], &[], None).await.expect("ready");
        let names = [workspace_pvc_name(id), docker_pvc_name(id)];

        let pod_closed = async {
            loop {
                if !db::sandbox_pod_is_live(&pool, pod).await.expect("live?") {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let dropped = tokio::time::timeout(Duration::from_secs(60), async {
            tokio::select! {
                biased;
                () = pod_closed => true,
                _ = remove_conversation_storage(&pool, id) => false,
            }
        })
        .await;
        assert_eq!(dropped, Ok(true), "the request is dropped midway, after the pod's row closed");
        for name in &names {
            assert!(pvc_api(&client).get_opt(name).await.expect("get_opt").is_some(), "{name} stood when the request went");
        }

        assert!(claims_gone(&client, &names).await, "the removal finished without its request");
        let failed = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let row = db::get_conversation_repo(&pool, repo.id).await.expect("get").expect("exists");
                if row.status != "ready" {
                    return row;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("the repo row was marked after the removal");
        assert!(failed.error.is_some(), "{failed:?}");
    })
    .await;
}

/// SME-253 review 1, finding 2: a claim left in place (not ours to delete)
/// is not a removed storage — the checkouts survive on it, so `ready` repos
/// stay ready and nothing says they were lost. The claim goes back into
/// the pre-SME-115 state (bound, then stripped of its instance label),
/// which is the state that actually keeps serving files.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_removal_that_deletes_nothing_leaves_ready_repos_ready(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT * 2, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let id = conversation.id;
        create_pod(&pool, id, PodLimitOverrides::default()).await.expect("create_pod");
        terminate_pod(&pool, id).await.expect("terminate_pod");
        let repo = db::create_conversation_repo(&pool, id, "u", &format!("k-{id}"), None, "r").await.expect("repo row");
        db::set_repo_ready(&pool, repo.id, 1, "main", None, &[], &[], None).await.expect("ready");

        let name = workspace_pvc_name(id);
        pvc_api(&client)
            .patch(
                &name,
                &PatchParams::default(),
                &Patch::Merge(serde_json::json!({"metadata": {"labels": {INSTANCE_LABEL: null}}})),
            )
            .await
            .expect("strip the instance label");

        let removal = remove_conversation_storage(&pool, id).await.expect("remove_conversation_storage");
        pvc_api(&client).delete(&name, &DeleteParams::default()).await.ok();

        assert_eq!((removal.claims_deleted, removal.claims_failed), (1, 1), "the docker claim went; the unlabelled workspace stayed: {removal:?}");
        let row = db::get_conversation_repo(&pool, repo.id).await.expect("get").expect("exists");
        assert_eq!(row.status, "ready", "a storage that survived kept its checkouts: the repo still reads ready: {row:?}");
        assert!(row.error.is_none(), "{row:?}");
    })
    .await;
}

/// SME-253 review 1, finding 3: a claim that outlives the removal's
/// confirm still announces itself when it goes — the chat panel refetches
/// on nothing else, and one refetch at the return would sit at
/// "removing…" forever.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_claim_outliving_the_removal_publishes_pods_changed_when_it_goes(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT * 2, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let id = conversation.id;
        create_pod(&pool, id, PodLimitOverrides::default()).await.expect("create_pod");
        terminate_pod(&pool, id).await.expect("terminate_pod");

        // A holder with no labels at all, invisible to the removal's own
        // waits, mounts the workspace claim — so its delete is still
        // finishing as the removal returns, past its 5 s confirm.
        let name = workspace_pvc_name(id);
        let holder: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"name": format!("slow-claim-holder-{id}")},
            "spec": {
                "restartPolicy": "Never",
                "containers": [{"name": "sleep", "image": OWN_SANDBOX_IMAGE, "command": ["sleep", "3600"], "imagePullPolicy": "IfNotPresent", "resources": {"requests": {"memory": "32Mi", "cpu": "10m"}, "limits": {"memory": "256Mi"}}}],
                "volumes": [{"name": "workspace", "persistentVolumeClaim": {"claimName": name}}],
            },
        }))
        .expect("pod");
        let holder_name = pods_api(&client)
            .create(&PostParams::default(), &holder)
            .await
            .expect("create the holder")
            .metadata
            .name
            .expect("a created pod's name");
        wait_holder_mounting(&client, &holder_name, &name).await;

        remove_conversation_storage(&pool, id).await.expect("remove_conversation_storage");
        // Non-vacuity: the holder really held it — the removal returned
        // with the claim still Terminating, which is the state the panel
        // would otherwise sit on.
        let held = pvc_api(&client).get(&name).await.expect("the holder held the claim past the removal");
        assert!(held.metadata.deletion_timestamp.is_some(), "the claim was still deleting when the removal returned");

        // Subscribe before freeing the holder: the announcement comes at
        // most a poll after the claim goes, and this can't miss it.
        let mut rx = crate::events::subscribe_app();
        pods_api(&client).delete(&holder_name, &immediate_delete_params()).await.expect("free the claim");

        // Any `PodsChanged` counts only once the claim has actually gone:
        // another test's can't answer for ours while it stands (the
        // broadcast is shared, so a stray event a full window after the
        // claim went is the one thing this can't rule out — kept tight).
        let settled = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match rx.recv().await {
                    Ok(crate::events::AppEvent::PodsChanged) => {
                        if pvc_api(&client).get_opt(&name).await.ok().flatten().is_none() {
                            return true;
                        }
                    }
                    Ok(_) => {}
                    Err(e) => panic!("the app event stream closed: {e}"),
                }
            }
        })
        .await
        .is_ok();
        assert!(settled, "a `PodsChanged` arrives when the slow claim finally goes");
        assert!(claims_gone(&client, &[name]).await, "the claim went too");
    })
    .await;
}

/// SME-253 review 1, finding 4: a doomed pre-SME-115 claim is never a
/// mount target either — even the database that owns unlabelled claims
/// waits it out and re-creates, rather than adopting one mid-deletion.
///
/// The claim is made by the pod lifecycle (only a Bound claim is ever
/// protected — pvc-protection skips unbound ones, and local-path binds
/// late), then stripped of its instance label to put it back into the
/// pre-SME-115 state.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_doomed_unlabelled_claim_is_waited_out_not_adopted(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT * 2, async {
        sqlx::query("UPDATE smelt_instance SET owns_unlabelled = true")
            .execute(&pool)
            .await
            .expect("a database that owns unlabelled objects");
        let instance = db::smelt_instance(&pool).await.expect("instance");
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let id = conversation.id;
        create_pod(&pool, id, PodLimitOverrides::default()).await.expect("create_pod");
        terminate_pod(&pool, id).await.expect("terminate_pod");

        // Bound, held by no pod, and unlabelled again — the state SME-115
        // left behind, which this database owns.
        let name = workspace_pvc_name(id);
        pvc_api(&client)
            .patch(
                &name,
                &PatchParams::default(),
                &Patch::Merge(serde_json::json!({"metadata": {"labels": {INSTANCE_LABEL: null}}})),
            )
            .await
            .expect("strip the instance label");
        let doomed_uid = pvc_api(&client)
            .get(&name)
            .await
            .expect("read the unlabelled claim")
            .metadata
            .uid
            .expect("a created claim's uid");
        let holder: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {
                "name": format!("doomed-unlabelled-holder-{id}"),
                "labels": {INSTANCE_LABEL: instance.id.clone()},
            },
            "spec": {
                "restartPolicy": "Never",
                "containers": [{"name": "sleep", "image": OWN_SANDBOX_IMAGE, "command": ["sleep", "3600"], "imagePullPolicy": "IfNotPresent", "resources": {"requests": {"memory": "32Mi", "cpu": "10m"}, "limits": {"memory": "256Mi"}}}],
                "volumes": [{"name": "workspace", "persistentVolumeClaim": {"claimName": name.clone()}}],
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
        wait_holder_mounting(&client, &holder_name, &name).await;

        pvc_api(&client).delete(&name, &DeleteParams::default()).await.expect("delete the claim");
        let doomed = pvc_api(&client).get(&name).await.expect("read the claim back");
        assert!(doomed.metadata.deletion_timestamp.is_some(), "the holder should leave it Terminating, not gone");

        // The holder goes a moment after the wait begins, as test 4's does.
        let freer = client.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            pods_api(&freer).delete(&holder_name, &immediate_delete_params()).await.ok();
        });

        ensure_conversation_pvcs(&client, id, &instance)
            .await
            .expect("a doomed unlabelled claim is waited out and re-created, not adopted mid-deletion");
        let fresh = pvc_api(&client).get(&name).await.expect("get the fresh claim");
        assert_ne!(fresh.metadata.uid.clone().expect("the fresh claim's uid"), doomed_uid, "the next pod gets a fresh claim, never the doomed one");
        assert!(fresh.metadata.deletion_timestamp.is_none(), "the re-created claim isn't being deleted");
    })
    .await;
}
