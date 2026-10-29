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
        let names = [workspace_pvc_name(id), docker_pvc_name(id)];
        // Stopping frees no disk: both claims stay until removed.
        for name in &names {
            assert!(pvc_api(&client).get_opt(name).await.expect("get_opt").is_some(), "{name} outlives the pod");
        }

        let removal = remove_conversation_storage(&pool, id).await.expect("remove_conversation_storage");
        assert!(!removal.pod_stopped, "no live pod to stop");
        assert_eq!((removal.claims_deleted, removal.claims_failed), (2, 0), "{removal:?}");
        assert!(claims_gone(&client, &names).await, "a stopped conversation's removal leaves no claims");
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
                "containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}],
                "volumes": [{"name": "workspace", "persistentVolumeClaim": {"claimName": name}}],
            },
        }))
        .expect("pod");
        pods_api(&client).create(&PostParams::default(), &holder).await.expect("create the LSP holder");

        let removal = remove_conversation_storage(&pool, id).await.expect("remove_conversation_storage");
        assert_eq!(removal.lsp_pods_stopped, 1, "the conversation's language-server pod was deleted");
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
