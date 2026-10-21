use super::*;

/// Makes a repo in pod `pod` whose `AGENTS.md` says `Run make test.`, as
/// a bare origin at `origin`, and returns its commit. The commit names its
/// own author: the test stores no git identity.
async fn make_origin(client: &kube::Client, pod: i64, origin: &str) -> String {
    let script = format!(
        "set -e; git init -q -b main /tmp/src; cd /tmp/src; echo 'Run make test.' > AGENTS.md; \
         git add AGENTS.md; git -c user.name=t -c user.email=t@example.com commit -qm one; git clone -q --bare /tmp/src {origin}; git rev-parse HEAD"
    );
    let made = exec_with(client, &pod_name(pod), "sandbox", &["sh", "-c", &script], None)
        .await
        .expect("exec make origin");
    assert_eq!(made.exit_code, 0, "make origin: {}{}", made.stdout, made.stderr);
    made.stdout.trim().to_string()
}

/// Runs `script` in pod `pod`'s sandbox container, and asserts it worked.
async fn sh(client: &kube::Client, pod: i64, script: &str) {
    let ran = exec_with(client, &pod_name(pod), "sandbox", &["sh", "-c", script], None)
        .await
        .expect("exec");
    assert_eq!(ran.exit_code, 0, "{script}: {}", ran.stderr);
}

/// SME-32: `/workspace` is the conversation's own. The next pod has the
/// checkouts, their uncommitted work, the repo list and the instructions
/// loaded from them.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_conversations_workspace_outlives_its_pod(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let pod = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        make_origin(&client, pod, "/tmp/origin.git").await;
        for dir in [None, Some("again")] {
            crate::git::clone_repo(&pool, conversation.id, "file:///tmp/origin.git", None, dir)
                .await
                .expect("clone_repo");
        }
        // A failed clone's record outlives the pod too.
        crate::git::clone_repo(&pool, conversation.id, "file:///tmp/origin.git", Some("nope"), Some("failed"))
            .await
            .expect_err("there's no branch nope");
        // Loaded, then edited in the checkout, alongside a file not
        // committed yet.
        crate::git::load_instructions(&pool, conversation.id, "/workspace/origin/AGENTS.md")
            .await
            .expect("load_instructions");
        let repos = crate::git::list_repos(&pool, conversation.id).await.expect("list repos");
        let shown = &repos[0].trust_requests[0];
        crate::git::decide_trust(&pool, conversation.id, shown.id, &shown.hash, true).await.expect("trust");
        sh(
            &client,
            pod,
            "echo 'not pushed yet' > /workspace/origin/uncommitted.txt && echo 'Run make lint too.' >> /workspace/origin/AGENTS.md",
        )
        .await;
        terminate_pod(&pool, conversation.id).await.expect("terminate the first pod");

        let next = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("a second pod");
        let kept = exec_with(
            &client,
            &pod_name(next),
            "sandbox",
            &["sh", "-c", "cat /workspace/origin/uncommitted.txt && git -C /workspace/origin status --porcelain"],
            None,
        )
        .await
        .expect("exec check workspace");
        assert_eq!(
            (kept.exit_code, kept.stdout.as_str()),
            (0, "not pushed yet\n M AGENTS.md\n?? uncommitted.txt\n"),
            "the checkout and its uncommitted file survive the pod: {}",
            kept.stderr
        );
        let repos = crate::git::list_repos(&pool, conversation.id).await.expect("list repos");
        assert_eq!(repos.len(), 3, "origin, again, failed: {repos:?}");
        assert_eq!(repos[0].status, crate::git::RepoStatus::Ready, "{repos:?}");
        assert_eq!(repos[0].loaded_instructions, vec!["AGENTS.md".to_string()], "{repos:?}");
        assert_eq!(repos[1].status, crate::git::RepoStatus::Ready, "{repos:?}");
        assert_eq!(repos[2].status, crate::git::RepoStatus::Failed, "{repos:?}");
        terminate_pod(&pool, conversation.id).await.expect("terminate the second pod");
        let pods_after = list_pods(&pool, conversation.id).await.expect("list_pods");
        assert!(pods_after.is_empty(), "no pods should be listed after terminating it, got {pods_after:?}");
    })
    .await;
}

/// SME-32, SME-49: "work on a repo" and the model's `clone_repo`, on a
/// conversation with no sandbox, each start one. The repo is recorded
/// first, so a turn started while the sandbox starts waits for the clone.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_attach_repo_and_clone_repo_start_a_sandbox_when_there_is_none(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    // Three pod starts, one after another.
    run_then_tear_down(&pool, &client, Duration::from_secs(300), async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        // An origin in /workspace, which outlives this pod, for the model's
        // clone below.
        let setup = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        make_origin(&client, setup, "/workspace/seed.git").await;
        terminate_pod(&pool, conversation.id).await.expect("terminate the setup pod");

        let attaching = tokio::spawn({
            let pool = pool.clone();
            let id = conversation.id;
            async move { crate::git::attach_repo(&pool, id, "file:///tmp/missing.git", None).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !crate::git::wait_for_clones(&pool, conversation.id, Duration::ZERO).await,
            "a turn started while the sandbox starts should see the clone coming"
        );
        let attached = attaching.await.expect("attach task").expect_err("this origin doesn't exist");
        assert!(attached.contains("does not appear to be a git repository"), "{attached}");
        // The user named it, so it counts as trusted.
        assert_eq!(db::get_repo_trust(&pool, "file/tmp/missing").await.expect("trust"), Some(true));
        assert_eq!(list_pods(&pool, conversation.id).await.expect("list_pods").len(), 1, "attach_repo started a sandbox");
        terminate_pod(&pool, conversation.id).await.expect("terminate the attach pod");

        let cloning = tokio::spawn({
            let pool = pool.clone();
            let id = conversation.id;
            async move { crate::git::clone_repo(&pool, id, "file:///workspace/seed.git", None, None).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !crate::git::wait_for_clones(&pool, conversation.id, Duration::ZERO).await,
            "a turn started while the sandbox starts should see the clone coming"
        );
        let seed = cloning.await.expect("clone task").expect("clone_repo starts the sandbox and clones");
        assert_eq!(seed.path, "/workspace/seed");
        assert_eq!(seed.status, crate::git::RepoStatus::Ready, "{seed:?}");
        assert_eq!(list_pods(&pool, conversation.id).await.expect("list_pods").len(), 1, "clone_repo started a sandbox");
        terminate_pod(&pool, conversation.id).await.expect("terminate the clone pod");
        assert!(
            list_terminals(&pool, conversation.id).await.expect("list_terminals").is_empty(),
            "no terminals should be listed after terminating all of them"
        );
    })
    .await;
}

/// SME-32: `clone_repo` records the repo and checks it out; its
/// `AGENTS.md` is listed but loads only once the user trusts it, and an
/// edit stays unloaded until the model loads it again. A failed clone
/// retried asks for what's asked now, a directory holding work is never
/// replaced, an empty repo clones, and asking again returns the same
/// checkout.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_clone_repo_checks_out_loads_instructions_and_retries_safely(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let id = conversation.id;
        let pod = create_pod(&pool, id, PodLimitOverrides::default()).await.expect("create_pod");

        let commit = make_origin(&client, pod, "/tmp/origin.git").await;
        let repo = crate::git::clone_repo(&pool, id, "file:///tmp/origin.git", None, None).await.expect("clone_repo");
        assert_eq!(repo.path, "/workspace/origin");
        assert_eq!(repo.status, crate::git::RepoStatus::Ready);
        assert_eq!(repo.commit.as_deref(), Some(commit.as_str()));
        assert_eq!(repo.branch.as_deref(), Some("main"));
        // The clone lists its AGENTS.md; nothing loads by itself.
        assert_eq!(repo.agents_files, vec!["AGENTS.md".to_string()]);
        assert!(crate::git::project_instructions(&pool, id).await.expect("loaded").is_empty());
        // The model asks to load it: a remote the user hasn't decided
        // about, so they're asked, with exactly that file.
        let asked = crate::git::load_instructions(&pool, id, "/workspace/origin/AGENTS.md")
            .await
            .expect("load_instructions");
        assert!(asked.contains("being asked"), "{asked}");
        let repos = crate::git::list_repos(&pool, id).await.expect("list repos");
        assert_eq!(repos[0].trust_requests.len(), 1);
        assert_eq!(repos[0].trust_requests[0].content, "Run make test.\n");
        // Trusted, that file is in the model's context.
        let shown = &repos[0].trust_requests[0];
        crate::git::decide_trust(&pool, id, shown.id, &shown.hash, true).await.expect("trust");
        let loaded = crate::git::project_instructions(&pool, id).await.expect("project instructions");
        assert_eq!(loaded.len(), 1, "{loaded:?}");
        assert_eq!(loaded[0].content, "Run make test.\n");
        assert_eq!(loaded[0].path, "/workspace/origin/AGENTS.md");
        assert_eq!(loaded[0].commit, repo.commit);

        // A failed clone retried with another branch (or URL) clones what's
        // asked for now, not what failed (SME-32 code review, finding 1).
        let failed = crate::git::clone_repo(&pool, id, "file:///tmp/origin.git", Some("nope"), Some("retry"))
            .await
            .expect_err("there's no branch nope");
        assert!(failed.contains("nope"), "{failed}");
        let retried = crate::git::clone_repo(&pool, id, "file:///tmp/origin.git", None, Some("retry"))
            .await
            .expect("the retry clones the default branch");
        assert_eq!(retried.requested_branch, None);
        assert_eq!(retried.branch.as_deref(), Some("main"));
        assert_eq!(retried.status, crate::git::RepoStatus::Ready);

        // A clone refused because the directory already holds work doesn't
        // delete that work when retried; only a clone that was interrupted
        // gets its directory replaced (SME-32 code review 2, finding 1).
        sh(&client, pod, "mkdir -p /workspace/mywork && echo precious > /workspace/mywork/notes.txt").await;
        let notes = || async {
            exec_with(&client, &pod_name(pod), "sandbox", &["cat", "/workspace/mywork/notes.txt"], None)
                .await
                .expect("exec cat")
                .stdout
        };
        for attempt in ["first", "retry"] {
            let refused = crate::git::clone_repo(&pool, id, "file:///tmp/origin.git", None, Some("mywork"))
                .await
                .expect_err("the directory holds work");
            assert!(refused.contains("already exists"), "{attempt}: {refused}");
        }
        assert_eq!(notes().await, "precious\n", "a retry must not delete the work");
        let mywork = db::list_conversation_repos(&pool, id)
            .await
            .expect("list")
            .into_iter()
            .find(|r| r.dir == "mywork")
            .expect("the mywork repo");
        // Even marked interrupted: that directory held work before any
        // clone, so a retry leaves it (SME-32 code review 3).
        db::set_repo_failed(&pool, mywork.id, mywork.attempt, crate::git::CLONE_INTERRUPTED)
            .await
            .expect("mark interrupted");
        let refused = crate::git::clone_repo(&pool, id, "file:///tmp/origin.git", None, Some("mywork"))
            .await
            .expect_err("the directory still holds work");
        assert!(refused.contains("already exists"), "{refused}");
        assert_eq!(notes().await, "precious\n");

        // A brand-new empty repo clones: its branch, no commit yet.
        sh(&client, pod, "git init -q --bare -b main /tmp/empty.git").await;
        let cloned_empty = crate::git::clone_repo(&pool, id, "file:///tmp/empty.git", None, None)
            .await
            .expect("an empty repo clones");
        assert_eq!(cloned_empty.status, crate::git::RepoStatus::Ready);
        assert_eq!((cloned_empty.branch.as_deref(), cloned_empty.commit.as_deref()), (Some("main"), None));

        // Asking again returns the same checkout rather than a second clone.
        let again = crate::git::clone_repo(&pool, id, "file:///tmp/origin.git", None, None).await.expect("clone_repo again");
        assert_eq!(again.id, repo.id);

        // AGENTS.md changes in the checkout: what's loaded stays until the
        // model loads it again, which a trusted repo allows at once.
        sh(&client, pod, "echo 'Run make lint too.' >> /workspace/origin/AGENTS.md").await;
        let loaded = crate::git::project_instructions(&pool, id).await.expect("loaded");
        assert_eq!(loaded[0].content, "Run make test.\n", "still the loaded version");
        let reloaded = crate::git::load_instructions(&pool, id, "/workspace/origin/AGENTS.md").await.expect("load again");
        assert!(reloaded.starts_with("Loaded"), "{reloaded}");
        let loaded = crate::git::project_instructions(&pool, id).await.expect("loaded");
        assert_eq!(loaded.len(), 1, "replaced, not added: {loaded:?}");
        assert_eq!(loaded[0].content, "Run make test.\nRun make lint too.\n");
    })
    .await;
}
