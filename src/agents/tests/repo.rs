//! A child on one of its parent's checkouts: trust, the clone at its ref
//! and its AGENTS.md, against a fake checkout.

use super::*;
use crate::git::CloneTarget;

const URL: &str = "https://example.com/team/app.git";
const KEY: &str = "example.com/team/app";
pub(super) const COMMIT: &str = "c0ffee0000000000000000000000000000000001";

/// A fake `ChildCheckout`: records each clone's target and makes the repo
/// ready (with an AGENTS.md when `agents_md` is set), or fails it.
#[derive(Default)]
pub(super) struct FakeCheckout {
    pub agents_md: Option<String>,
    pub clone_error: Option<String>,
    /// Held here until notified, when set.
    pub hold_clone: Option<Arc<tokio::sync::Notify>>,
    pub targets: Mutex<Vec<CloneTarget>>,
    /// What `parent_commit` answers; refused when unset.
    pub parent: Option<Result<(String, String), String>>,
    /// The parent's persona files, relative to its checkout.
    pub parent_personas: Vec<String>,
    /// Other files in the child's checkout, by path relative to it.
    pub files: Mutex<std::collections::HashMap<String, String>>,
}

impl ChildCheckout for FakeCheckout {
    fn parent_commit<'a>(
        &'a self,
        _pool: &'a PgPool,
        _parent: i64,
        _dir: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(String, String), String>> + Send + 'a>> {
        Box::pin(async move { self.parent.clone().unwrap_or_else(|| Err("this fake has no parent commit".to_string())) })
    }

    fn parent_persona_paths<'a>(
        &'a self,
        _pool: &'a PgPool,
        _parent: i64,
        _dir: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<String>, String>> + Send + 'a>> {
        Box::pin(async move { Ok(self.parent_personas.clone()) })
    }

    fn clone_repo<'a>(
        &'a self,
        pool: &'a PgPool,
        _child: i64,
        repo: &'a db::ConversationRepo,
        target: &'a CloneTarget,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.targets.lock().expect("targets").push(target.clone());
            if let Some(hold) = &self.hold_clone {
                hold.notified().await;
            }
            if let Some(error) = &self.clone_error {
                db::set_repo_failed(pool, repo.id, repo.attempt, error).await.expect("fail the repo");
                return Err(error.clone());
            }
            let agents_files: Vec<String> = self.agents_md.iter().map(|_| "AGENTS.md".to_string()).collect();
            let branch = match target {
                CloneTarget::Ref(r) => r,
                CloneTarget::Commit { branch, .. } => branch,
            };
            db::set_repo_ready(pool, repo.id, repo.attempt, branch, Some(COMMIT), &agents_files, &[], None)
                .await
                .expect("make the repo ready");
            Ok(())
        })
    }

    fn read_file<'a>(
        &'a self,
        _pool: &'a PgPool,
        _child: i64,
        _dir: &'a str,
        rel: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<db::InstructionsFile>, String>> + Send + 'a>> {
        Box::pin(async move {
            let content = match rel {
                "AGENTS.md" => self.agents_md.clone(),
                rel => self.files.lock().expect("files").get(rel).cloned(),
            };
            Ok(content.map(|content| db::InstructionsFile {
                file_bytes: content.len() as i64,
                content,
                hash: "h".to_string(),
                commit: None,
            }))
        })
    }
}

/// A root with a ready checkout of `URL` at /workspace/app, trusted as
/// `trust` says, using `fake` for its children's starts.
pub(super) async fn root_with_checkout(pool: &PgPool, trust: Option<bool>, fake: Arc<FakeCheckout>) -> i64 {
    let root = db::create_conversation(pool).await.expect("create conversation").id;
    let repo = db::create_conversation_repo(pool, root, URL, KEY, None, "app").await.expect("a repo row");
    db::set_repo_cloned(pool, repo.id, "main", Some(COMMIT)).await.expect("ready");
    if let Some(trusted) = trust {
        db::set_repo_trust(pool, KEY, trusted).await.expect("trust");
    }
    test_fakes::register(root, fake);
    root
}

pub(super) async fn start_on(pool: &PgPool, root: i64, input: serde_json::Value) -> Result<String, String> {
    crate::anthropic::tools::execute(pool, root, "toolu_s", "start_agent", &input).await
}

pub(super) async fn children(pool: &PgPool, root: i64) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM conversations WHERE parent_id = $1")
        .bind(root)
        .fetch_one(pool)
        .await
        .expect("count")
}

async fn child_repo(pool: &PgPool, child: i64) -> db::ConversationRepo {
    db::list_conversation_repos(pool, child).await.expect("repos").into_iter().next().expect("the child's repo")
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_undecided_or_declined_remote_is_refused_before_anything_is_written(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let undecided = root_with_checkout(&pool, None, Arc::new(FakeCheckout::default())).await;
    let input = serde_json::json!({"brief": "Fix it.", "repo": "/workspace/app", "ref": "main"});
    let refused = start_on(&pool, undecided, input.clone()).await.expect_err("refused");
    assert!(refused.contains("hasn't decided whether to trust https://example.com/team/app.git"), "{refused}");
    // Review 1, finding 6: a repo with no AGENTS.md has no card to ask with,
    // so the refusal names the user's own way to trust it too.
    assert!(refused.contains("\"Work on a repo\""), "{refused}");
    assert_eq!(children(&pool, undecided).await, 0);

    db::set_repo_trust(&pool, KEY, false).await.expect("decline");
    let refused = start_on(&pool, undecided, input).await.expect_err("refused");
    assert!(refused.contains("declined to trust https://example.com/team/app.git"), "{refused}");
    assert_eq!(children(&pool, undecided).await, 0);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_child_clones_at_its_ref_and_loads_its_agents_md(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("ok")]).await;
    let hold = Arc::new(tokio::sync::Notify::new());
    let fake = Arc::new(FakeCheckout {
        agents_md: Some("Run the tests.".to_string()),
        hold_clone: Some(hold.clone()),
        ..Default::default()
    });
    let root = root_with_checkout(&pool, Some(true), fake.clone()).await;
    let result = start_on(&pool, root, serde_json::json!({"brief": "Fix it.", "repo": "/workspace/app/", "ref": "release-1"}))
        .await
        .expect("started");
    let child = child_of(&pool, root, &result).await;
    assert!(result.contains("on https://example.com/team/app.git at release-1"), "{result}");
    let cloning = child_repo(&pool, child).await;
    assert_eq!((cloning.status.as_str(), cloning.dir.as_str(), cloning.url.as_str()), ("cloning", "app", URL));
    assert_eq!(cloning.branch.as_deref(), Some("release-1"));
    eventually("the clone is under way", || async { !fake.targets.lock().expect("targets").is_empty() }).await;
    assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_STARTING));

    hold.notify_one();
    eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
    assert_eq!(*fake.targets.lock().expect("targets"), vec![CloneTarget::Ref("release-1".to_string())]);
    assert_eq!(child_repo(&pool, child).await.status, "ready");
    let loaded = db::list_loaded_instructions(&pool, child).await.expect("loaded");
    assert_eq!(loaded.len(), 1, "{loaded:?}");
    assert_eq!((loaded[0].path.as_str(), loaded[0].content.as_str()), ("AGENTS.md", "Run the tests."));
    assert_eq!(loaded[0].commit_sha.as_deref(), Some(COMMIT));
    assert!(db::list_instruction_requests(&pool, child).await.expect("cards").is_empty(), "no card: it's trusted");
    eventually("the child's first turn ran", || async { !requests_for(&requests, Some(child)).is_empty() }).await;
    let system = requests_for(&requests, Some(child))[0]["system"].to_string();
    assert!(system.contains("Run the tests."), "its AGENTS.md is in its prompt");
    eventually("every turn is over", || async { !crate::turn::turn_running(child) && !crate::turn::turn_running(root) }).await;
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_clone_failure_is_the_report(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let fake = Arc::new(FakeCheckout { clone_error: Some("no branch, tag or commit `nope` on x".to_string()), ..Default::default() });
    let root = root_with_checkout(&pool, Some(true), fake).await;
    let result = start_on(&pool, root, serde_json::json!({"brief": "Fix it.", "repo": "/workspace/app", "ref": "nope"}))
        .await
        .expect("started");
    let child = child_of(&pool, root, &result).await;
    eventually("the start fails", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_FAILED) }).await;
    let report = format!("Report from agent #{child}:\n\nagent #{child}'s start failed: no branch, tag or commit `nope` on x");
    eventually("the parent gets the error as the report", || async {
        transcript(&pool, root).await.iter().any(|(_, text)| *text == report)
    })
    .await;
    assert_eq!(child_repo(&pool, child).await.status, "failed");
    eventually("every turn is over", || async { !crate::turn::turn_running(root) }).await;
}

/// The user forgets the remote while the child clones: the start fails,
/// even for a checkout with no AGENTS.md to ask about.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_trust_forgotten_during_the_clone_fails_the_start(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let hold = Arc::new(tokio::sync::Notify::new());
    let fake = Arc::new(FakeCheckout { hold_clone: Some(hold.clone()), ..Default::default() });
    let root = root_with_checkout(&pool, Some(true), fake.clone()).await;
    let result = start_on(&pool, root, serde_json::json!({"brief": "Fix it.", "repo": "/workspace/app", "ref": "main"}))
        .await
        .expect("started");
    let child = child_of(&pool, root, &result).await;
    eventually("the clone is under way", || async { !fake.targets.lock().expect("targets").is_empty() }).await;
    db::delete_repo_trust(&pool, KEY).await.expect("forget");
    hold.notify_one();

    eventually("the start fails", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_FAILED) }).await;
    assert!(
        transcript(&pool, child).await.iter().any(|(_, text)| text.ends_with("start failed: the user no longer trusts https://example.com/team/app.git")),
        "{:?}",
        transcript(&pool, child).await
    );
    eventually("every turn is over", || async { !crate::turn::turn_running(root) }).await;
}

/// Forgotten between the start's trust read and its AGENTS.md load: the
/// load makes a card, the start fails, and its exit deletes the card.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_card_the_load_made_is_deleted_when_the_start_fails(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let fake = Arc::new(FakeCheckout { agents_md: Some("Rules.".to_string()), ..Default::default() });
    let root = root_with_checkout(&pool, Some(true), fake).await;
    let (reached, release) = test_hooks::pause(test_hooks::BEFORE_AGENTS_MD_LOAD, root);
    let result = start_on(&pool, root, serde_json::json!({"brief": "Fix it.", "repo": "/workspace/app", "ref": "main"}))
        .await
        .expect("started");
    let child = child_of(&pool, root, &result).await;
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the start runs").expect("paused");
    db::delete_repo_trust(&pool, KEY).await.expect("forget");
    release.send(()).expect("release the start");

    eventually("the start fails", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_FAILED) }).await;
    eventually("the start's exit ran", || async { test_hooks::starts_ended(child) }).await;
    assert!(db::list_instruction_requests(&pool, child).await.expect("cards").is_empty(), "the card is deleted");
    eventually("every turn is over", || async { !crate::turn::turn_running(root) }).await;
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_repo_must_be_a_ready_checkout_and_ref_must_be_plain(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = root_with_checkout(&pool, Some(true), Arc::new(FakeCheckout::default())).await;
    let refused = start_on(&pool, root, serde_json::json!({"brief": "x", "repo": "/workspace/other", "ref": "main"}))
        .await
        .expect_err("refused");
    assert_eq!(refused, "/workspace/other isn't one of your checkouts; yours: /workspace/app");
    for bad in ["-x", "a..b", "a b", "x.lock", "x/", "a@{1}", "a\u{7}"] {
        let refused = start_on(&pool, root, serde_json::json!({"brief": "x", "repo": "/workspace/app", "ref": bad}))
            .await
            .expect_err("refused");
        assert!(refused.contains("can't be a ref"), "{bad:?}: {refused}");
    }
    let refused = start_on(&pool, root, serde_json::json!({"brief": "x", "ref": "main"})).await.expect_err("refused");
    assert_eq!(refused, "ref needs repo: the checkout whose remote it's on");
    assert_eq!(children(&pool, root).await, 0);
}

/// Without a ref, the child gets its parent's exact commit, on a branch of
/// its upstream's name.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_without_a_ref_the_child_gets_its_parents_commit(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let sha = "abc1230000000000000000000000000000000002".to_string();
    let fake = Arc::new(FakeCheckout { parent: Some(Ok((sha.clone(), "feature".to_string()))), ..Default::default() });
    let root = root_with_checkout(&pool, Some(true), fake.clone()).await;
    let result = start_on(&pool, root, serde_json::json!({"brief": "Fix it.", "repo": "/workspace/app"}))
        .await
        .expect("started");
    let child = child_of(&pool, root, &result).await;
    assert!(result.contains(&format!("at your commit {sha} (branch feature)")), "{result}");
    eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
    assert_eq!(*fake.targets.lock().expect("targets"), vec![CloneTarget::Commit { sha, branch: "feature".to_string() }]);
    assert_eq!(child_repo(&pool, child).await.branch.as_deref(), Some("feature"));
    eventually("every turn is over", || async { !crate::turn::turn_running(child) && !crate::turn::turn_running(root) }).await;
}

/// A parent whose commit can't be shared is refused before anything is
/// written, with what to do.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_parent_commit_that_cant_be_shared_is_refused(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let why = "/workspace/app has 2 commits not on origin/main: push them, or pass ref".to_string();
    let fake = Arc::new(FakeCheckout { parent: Some(Err(why.clone())), ..Default::default() });
    let root = root_with_checkout(&pool, Some(true), fake).await;
    let refused = start_on(&pool, root, serde_json::json!({"brief": "Fix it.", "repo": "/workspace/app"}))
        .await
        .expect_err("refused");
    assert_eq!(refused, why);
    assert_eq!(children(&pool, root).await, 0);
}

/// Review 2, finding 2: the parent's commit and branch come from its own
/// pod, which its model controls, so they're checked before they reach the
/// child's git as arguments.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_parent_commit_that_isnt_a_commit_id_or_branch_is_refused(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    for (sha, branch) in [("--upload-pack=touch /tmp/x", "main"), (COMMIT, "-x"), ("abc", "main")] {
        let fake = Arc::new(FakeCheckout { parent: Some(Ok((sha.to_string(), branch.to_string()))), ..Default::default() });
        let root = root_with_checkout(&pool, Some(true), fake).await;
        let refused = start_on(&pool, root, serde_json::json!({"brief": "x", "repo": "/workspace/app"}))
            .await
            .expect_err("refused");
        assert!(refused.contains("isn't a commit id") || refused.contains("can't be a ref"), "{sha} {branch}: {refused}");
        assert_eq!(children(&pool, root).await, 0);
    }
}
