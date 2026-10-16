use super::*;

#[sqlx::test]
async fn test_create_key_stores_it_and_returns_only_the_public_half(pool: PgPool) {
    let created = create_key(&pool, "github").await.expect("create key");
    assert_eq!(created.name, "github");
    assert!(created.public_key.starts_with("ssh-ed25519 "), "{}", created.public_key);
    assert_eq!(created.fingerprint, fingerprint(&created.public_key).expect("fp"));

    let stored = db::list_ssh_keys(&pool).await.expect("list");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].public_key, created.public_key);
    assert!(stored[0].private_key.contains("OPENSSH PRIVATE KEY"));
    assert_eq!(list_keys(&pool).await.expect("list keys"), vec![created]);
}

#[sqlx::test]
async fn test_create_key_refuses_an_unsafe_name(pool: PgPool) {
    let unsafe_name = create_key(&pool, "../x").await.expect_err("unsafe");
    assert!(unsafe_name.contains("letters"), "{unsafe_name}");
    assert!(db::list_ssh_keys(&pool).await.expect("list").is_empty());
}

/// One key at a time, for now: with several, the host picks the
/// first it knows, which breaks per-repo deploy keys (SME-32 code
/// review 3).
#[sqlx::test]
async fn test_only_one_key_at_a_time(pool: PgPool) {
    let first = create_key(&pool, "github").await.expect("first");
    let generated = create_key(&pool, "another").await.expect_err("a second key");
    assert!(generated.contains("Delete it first"), "{generated}");
    let imported = import_key_named(&pool, "imported", &generate_key("x").private_key)
        .await
        .expect_err("a second key, imported");
    assert!(imported.contains("Delete it first"), "{imported}");
    // The database refuses one too, whatever the code checks.
    assert!(db::create_ssh_key(&pool, "sneaky", "pub", "priv").await.is_err());
    assert_eq!(list_keys(&pool).await.expect("list").len(), 1);

    delete_key(&pool, first.id).await.expect("delete");
    create_key(&pool, "replacement").await.expect("a key again once there's none");
}

#[sqlx::test]
async fn test_import_key_stores_the_derived_public_half(pool: PgPool) {
    let pair = generate_key("elsewhere");
    let imported = import_key_named(&pool, "laptop", &pair.private_key)
        .await
        .expect("import");
    assert_eq!(imported.name, "laptop");
    assert_eq!(imported.public_key, pair.public_key);
    let refused = import_key_named(&pool, "bad", "nope").await.expect_err("garbage");
    assert!(refused.contains("private key"), "{refused}");
}

#[sqlx::test]
async fn test_clone_repo_refuses_before_recording_anything(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let bad_url = clone_repo(&pool, conversation.id, "not a url", None, None)
        .await
        .expect_err("bad url");
    assert!(bad_url.contains("isn't a git URL"), "{bad_url}");
    let bad_dir = clone_repo(&pool, conversation.id, "git@github.com:o/r.git", None, Some("../etc"))
        .await
        .expect_err("bad dir");
    assert!(bad_dir.contains("directory"), "{bad_dir}");
    assert!(list_repos(&pool, conversation.id).await.expect("list").is_empty());
}

#[test]
fn test_checkout_dirs_are_one_plain_name() {
    // A repo like `org/.github` gets its own name by default.
    let github = default_checkout_dir("git@github.com:org/.github.git").expect("a dir");
    for ok in ["smelt", "my.repo", "repo-2", "a_b", ".github", github.as_str()] {
        assert!(validate_checkout_dir(ok).is_ok(), "{ok}");
    }
    for bad in ["", ".", "..", "../x", "a/b", "-rf", "has space", &"x".repeat(101)] {
        assert!(validate_checkout_dir(bad).is_err(), "{bad:?}");
    }
}

#[sqlx::test]
async fn test_wait_for_clones_waits_for_a_clone_to_finish(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    // Nothing cloning: no wait.
    assert!(wait_for_clones(&pool, conversation.id, std::time::Duration::from_secs(5)).await);
    let repo = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
        .await
        .expect("repo");
    // Still cloning at the deadline.
    assert!(!wait_for_clones(&pool, conversation.id, std::time::Duration::from_millis(300)).await);
    let finisher = {
        let pool = pool.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            db::set_repo_cloned(&pool, repo.id, "main", Some("abc")).await.expect("cloned");
        })
    };
    let started = std::time::Instant::now();
    assert!(wait_for_clones(&pool, conversation.id, std::time::Duration::from_secs(10)).await);
    assert!(started.elapsed() >= std::time::Duration::from_millis(250), "it waited");
    finisher.await.expect("finisher");
}

pub(super) fn file(content: &str) -> db::InstructionsFile {
    db::InstructionsFile {
        content: content.to_string(),
        file_bytes: content.len() as i64,
        hash: format!("hash-{content}"),
        commit: Some("abc123".to_string()),
    }
}

pub(super) async fn cloned_repo(pool: &PgPool, conversation_id: i64) -> db::ConversationRepo {
    let repo = db::create_conversation_repo(pool, conversation_id, "git@github.com:o/r.git", "github.com/o/r", None, "r")
        .await
        .expect("repo");
    db::set_repo_cloned(pool, repo.id, "main", Some("abc123")).await.expect("cloned");
    db::get_conversation_repo(pool, repo.id).await.expect("get").expect("exists")
}

pub(super) async fn only_repo(pool: &PgPool, conversation_id: i64) -> RepoSummary {
    let mut repos = list_repos(pool, conversation_id).await.expect("list");
    assert_eq!(repos.len(), 1, "{repos:?}");
    repos.remove(0)
}

#[sqlx::test]
async fn test_a_trusted_repos_agents_md_loads_at_once(pool: PgPool) {
    db::set_repo_trust(&pool, "github.com/o/r", true).await.expect("trust");
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let repo = cloned_repo(&pool, conversation.id).await;
    let outcome = request_or_load(&pool, conversation.id, &repo, "web/AGENTS.md", &file("Use pnpm.\n"))
        .await
        .expect("load");
    assert_eq!(outcome, LoadOutcome::Loaded);
    let loaded = project_instructions(&pool, conversation.id).await.expect("loaded");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].path, "/workspace/r/web/AGENTS.md");
    assert_eq!(loaded[0].content, "Use pnpm.\n");
    assert_eq!(loaded[0].commit.as_deref(), Some("abc123"));
    assert_eq!(only_repo(&pool, conversation.id).await.loaded_instructions, vec!["web/AGENTS.md".to_string()]);
}

#[sqlx::test]
async fn test_an_unknown_repo_asks_the_user_with_the_exact_file(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let repo = cloned_repo(&pool, conversation.id).await;
    let outcome = request_or_load(&pool, conversation.id, &repo, "AGENTS.md", &file("Run make test.\n"))
        .await
        .expect("ask");
    assert_eq!(outcome, LoadOutcome::AwaitingTrust);
    assert!(project_instructions(&pool, conversation.id).await.expect("loaded").is_empty());
    let summary = only_repo(&pool, conversation.id).await;
    assert_eq!(summary.trust_requests.len(), 1);
    assert_eq!(summary.trust_requests[0].path, "/workspace/r/AGENTS.md");
    assert_eq!(summary.trust_requests[0].content, "Run make test.\n");
}

#[sqlx::test]
async fn test_a_declined_repos_agents_md_is_refused(pool: PgPool) {
    db::set_repo_trust(&pool, "github.com/o/r", false).await.expect("decline");
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let repo = cloned_repo(&pool, conversation.id).await;
    let refused = request_or_load(&pool, conversation.id, &repo, "AGENTS.md", &file("x"))
        .await
        .expect_err("declined");
    assert!(refused.contains("doesn't trust"), "{refused}");
    let summary = only_repo(&pool, conversation.id).await;
    assert!(summary.trust_requests.is_empty() && summary.loaded_instructions.is_empty());
}

#[sqlx::test]
async fn test_trust_loads_exactly_the_file_the_card_showed(pool: PgPool) {
    let first = db::create_conversation(&pool).await.expect("conversation");
    let second = db::create_conversation(&pool).await.expect("conversation");
    let repo = cloned_repo(&pool, first.id).await;
    let other = cloned_repo(&pool, second.id).await;
    request_or_load(&pool, first.id, &repo, "AGENTS.md", &file("what the user read")).await.expect("ask");
    // Another conversation's copy, which the user never saw.
    request_or_load(&pool, second.id, &other, "AGENTS.md", &file("injected")).await.expect("ask");
    let shown = only_repo(&pool, first.id).await.trust_requests.remove(0);

    let notices = decide_trust(&pool, first.id, shown.id, &shown.hash, true).await.expect("trust");
    assert_eq!(notices[0].0, first.id, "the deciding conversation's notice comes first");
    let notice = notices[0].1.clone();
    assert!(notice.contains("/workspace/r/AGENTS.md"), "{notice}");
    assert_eq!(db::get_repo_trust(&pool, "github.com/o/r").await.expect("get"), Some(true));
    let loaded = project_instructions(&pool, first.id).await.expect("loaded");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].content, "what the user read");
    assert!(only_repo(&pool, first.id).await.trust_requests.is_empty());

    // The unseen copy isn't loaded; that conversation is told to ask again.
    assert!(project_instructions(&pool, second.id).await.expect("loaded").is_empty());
    assert!(only_repo(&pool, second.id).await.trust_requests.is_empty());
    assert!(
        notices.iter().any(|(id, text)| *id == second.id && text.contains("load_instructions again")),
        "that conversation is told to ask again: {notices:?}"
    );
}

/// The file changed (the model asked again) while the card was
/// open: Trust on what the user read doesn't load the newer copy
/// (SME-32 code review 5, finding 1).
#[sqlx::test]
async fn test_trust_refuses_a_file_that_changed_since_it_was_shown(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let repo = cloned_repo(&pool, conversation.id).await;
    request_or_load(&pool, conversation.id, &repo, "AGENTS.md", &file("what the user read")).await.expect("ask");
    let shown = only_repo(&pool, conversation.id).await.trust_requests.remove(0);
    request_or_load(&pool, conversation.id, &repo, "AGENTS.md", &file("swapped in")).await.expect("ask again");

    let refused = decide_trust(&pool, conversation.id, shown.id, &shown.hash, true)
        .await
        .expect_err("the file changed");
    assert!(refused.contains("changed"), "{refused}");
    assert!(project_instructions(&pool, conversation.id).await.expect("loaded").is_empty());
    assert_eq!(db::get_repo_trust(&pool, "github.com/o/r").await.expect("get"), None, "nothing decided");
    let newer = only_repo(&pool, conversation.id).await.trust_requests.remove(0);
    assert_eq!(newer.content, "swapped in", "the card shows the newer copy");

    decide_trust(&pool, conversation.id, newer.id, &newer.hash, true).await.expect("trust what's shown now");
    assert_eq!(project_instructions(&pool, conversation.id).await.expect("loaded")[0].content, "swapped in");
}

/// Deciding on one card drops the same conversation's other
/// requests for that repo; the model is told which (SME-32 code
/// review 5, finding 2).
#[sqlx::test]
async fn test_the_model_hears_about_its_other_requests_for_the_repo(pool: PgPool) {
    for trusted in [true, false] {
        let conversation = db::create_conversation(&pool).await.expect("conversation");
        let repo = cloned_repo(&pool, conversation.id).await;
        request_or_load(&pool, conversation.id, &repo, "AGENTS.md", &file("top")).await.expect("ask");
        request_or_load(&pool, conversation.id, &repo, "web/AGENTS.md", &file("web")).await.expect("ask");
        let requests = only_repo(&pool, conversation.id).await.trust_requests;
        assert_eq!(requests.len(), 2);
        let top = requests.iter().find(|r| r.path.ends_with("/r/AGENTS.md")).expect("top-level request");
        let notice = decide_trust(&pool, conversation.id, top.id, &top.hash, trusted).await.expect("decide").remove(0).1;
        assert!(notice.contains("/workspace/r/web/AGENTS.md"), "{trusted}: {notice}");
        if trusted {
            assert!(notice.contains("load_instructions again"), "{notice}");
        } else {
            assert!(notice.contains("won't be loaded"), "{notice}");
        }
        assert!(only_repo(&pool, conversation.id).await.trust_requests.is_empty());
        db::delete_repo_trust(&pool, "github.com/o/r").await.expect("reset for the next round");
    }
}

/// The decision is recorded at once, and the notices are the
/// caller's to deliver: a turn running in the conversation doesn't
/// hold up the user's click (SME-32 code review 6, finding 3).
#[sqlx::test]
async fn test_deciding_doesnt_wait_for_a_running_turn(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let repo = cloned_repo(&pool, conversation.id).await;
    request_or_load(&pool, conversation.id, &repo, "AGENTS.md", &file("x")).await.expect("ask");
    let shown = only_repo(&pool, conversation.id).await.trust_requests.remove(0);
    let lock = crate::turn::conversation_lock(conversation.id);
    let _running_turn = lock.lock().await;
    let decided = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        decide_trust(&pool, conversation.id, shown.id, &shown.hash, true),
    )
    .await
    .expect("the decision returns while a turn runs")
    .expect("trust");
    assert_eq!(decided[0].0, conversation.id);
    assert_eq!(project_instructions(&pool, conversation.id).await.expect("loaded").len(), 1);
}

/// "Work on a repo" on a repo the model already cloned still
/// counts as the user picking it (SME-32 code review 7, finding 4).
/// The Clone button's request only records: the sandbox and the
/// clone run after it, so a closed tab can't cut them off (SME-32
/// code review 8, finding 1).
#[sqlx::test]
async fn test_starting_work_on_a_repo_records_it_without_touching_a_pod(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let (shown, pending, _) = start_attach(&pool, conversation.id, "git@github.com:o/r.git", None, None)
        .await
        .expect("start");
    assert_eq!(shown.status, RepoStatus::Cloning);
    assert_eq!(pending.expect("still to clone").id, shown.id);
    assert_eq!(db::get_repo_trust(&pool, "github.com/o/r").await.expect("get"), Some(true));
    assert!(db::list_sandbox_pods(&pool, conversation.id).await.expect("pods").is_empty(), "no pod yet");
    assert!(!wait_for_clones(&pool, conversation.id, std::time::Duration::ZERO).await, "a turn waits for it");
}

/// "Work on a repo" never reaches a dead end (SME-32 code review
/// 10, finding 2): a failed clone's directory can be taken over by
/// a corrected URL, and a directory can be named for a second
/// checkout.
#[sqlx::test]
async fn test_work_on_a_repo_can_correct_a_url_or_name_a_directory(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let typo = db::create_conversation_repo(&pool, conversation.id, "git@github.com:me/app.git", "github.com/me/app", None, "app")
        .await
        .expect("repo");
    db::set_repo_failed(&pool, typo.id, typo.attempt, "ERROR: Repository not found.").await.expect("failed");

    let (shown, pending, _) = start_attach(&pool, conversation.id, "git@github.com:org/app.git", None, None)
        .await
        .expect("the corrected URL takes the failed clone's place");
    assert_eq!(shown.id, typo.id, "replaced, not added");
    assert_eq!(shown.url, "git@github.com:org/app.git");
    assert_eq!(pending.expect("to clone").remote_key, "github.com/org/app");

    // The same repo on another branch, in a directory of its own.
    db::set_repo_cloned(&pool, typo.id, "main", Some("abc")).await.expect("cloned");
    let (shown, _, _) = start_attach(&pool, conversation.id, "git@github.com:org/app.git", Some("dev"), Some("app-dev"))
        .await
        .expect("a second checkout");
    assert_eq!(shown.path, "/workspace/app-dev");
    assert_eq!(list_repos(&pool, conversation.id).await.expect("list").len(), 2);
}

#[sqlx::test]
async fn test_work_on_a_repo_trusts_an_existing_checkout(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    cloned_repo(&pool, conversation.id).await;
    // A live pod row: nothing needs starting.
    db::create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
    let repo = attach_repo(&pool, conversation.id, "git@github.com:o/r.git", None)
        .await
        .expect("the existing checkout");
    assert_eq!(repo.path, "/workspace/r");
    assert_eq!(db::get_repo_trust(&pool, "github.com/o/r").await.expect("get"), Some(true));
}

/// A remote trusted without its card being answered (the model
/// loading again once it's trusted, or "Work on a repo") leaves no
/// card behind, and every waiting model hears (SME-32 code review
/// 9, finding 1).
#[sqlx::test]
async fn test_trust_given_elsewhere_settles_waiting_requests(pool: PgPool) {
    let waiting = db::create_conversation(&pool).await.expect("conversation");
    let repo = cloned_repo(&pool, waiting.id).await;
    request_or_load(&pool, waiting.id, &repo, "AGENTS.md", &file("x")).await.expect("ask");

    // "Work on a repo" with the same remote, from another conversation.
    let attaching = db::create_conversation(&pool).await.expect("conversation");
    let (_, _, notices) = start_attach(&pool, attaching.id, "https://github.com/o/r", None, None)
        .await
        .expect("start");
    assert!(only_repo(&pool, waiting.id).await.trust_requests.is_empty(), "no card left behind");
    assert!(
        notices.iter().any(|(id, text)| *id == waiting.id && text.contains("load_instructions again")),
        "the waiting model hears: {notices:?}"
    );

    // A trusted load of a file with a request pending removes it.
    let other = db::create_conversation(&pool).await.expect("conversation");
    let other_repo = cloned_repo(&pool, other.id).await;
    db::delete_repo_trust(&pool, "github.com/o/r").await.expect("forget");
    request_or_load(&pool, other.id, &other_repo, "AGENTS.md", &file("y")).await.expect("ask");
    db::set_repo_trust(&pool, "github.com/o/r", true).await.expect("trusted meanwhile");
    assert_eq!(
        request_or_load(&pool, other.id, &other_repo, "AGENTS.md", &file("y")).await.expect("load"),
        LoadOutcome::Loaded
    );
    assert!(only_repo(&pool, other.id).await.trust_requests.is_empty(), "the stale card goes");
}

/// Declining unloads what was loaded from that remote before, in
/// every conversation, so no instructions from an untrusted repo
/// stay in a system prompt (SME-32 code review 9, finding 2).
#[sqlx::test]
async fn test_declining_unloads_what_was_loaded_before(pool: PgPool) {
    db::set_repo_trust(&pool, "github.com/o/r", true).await.expect("trust");
    let earlier = db::create_conversation(&pool).await.expect("conversation");
    let loaded_repo = cloned_repo(&pool, earlier.id).await;
    request_or_load(&pool, earlier.id, &loaded_repo, "AGENTS.md", &file("old")).await.expect("load");
    db::delete_repo_trust(&pool, "github.com/o/r").await.expect("forget");

    let deciding = db::create_conversation(&pool).await.expect("conversation");
    let repo = cloned_repo(&pool, deciding.id).await;
    request_or_load(&pool, deciding.id, &repo, "AGENTS.md", &file("new")).await.expect("ask");
    let shown = only_repo(&pool, deciding.id).await.trust_requests.remove(0);
    let notices = decide_trust(&pool, deciding.id, shown.id, &shown.hash, false).await.expect("decline");

    assert!(project_instructions(&pool, earlier.id).await.expect("loaded").is_empty(), "unloaded");
    assert!(
        notices.iter().any(|(id, text)| *id == earlier.id && text.contains("no longer")),
        "that model is told: {notices:?}"
    );
}

#[sqlx::test]
async fn test_declining_loads_nothing_and_later_loads_are_refused(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let repo = cloned_repo(&pool, conversation.id).await;
    request_or_load(&pool, conversation.id, &repo, "AGENTS.md", &file("x")).await.expect("ask");
    let shown = only_repo(&pool, conversation.id).await.trust_requests.remove(0);
    let notice = decide_trust(&pool, conversation.id, shown.id, &shown.hash, false).await.expect("decline").remove(0).1;
    assert!(notice.contains("not to trust"), "{notice}");
    assert!(project_instructions(&pool, conversation.id).await.expect("loaded").is_empty());
    assert!(only_repo(&pool, conversation.id).await.trust_requests.is_empty());
    assert!(request_or_load(&pool, conversation.id, &repo, "AGENTS.md", &file("x")).await.is_err());
}

#[sqlx::test]
async fn test_a_request_is_decided_only_from_its_own_conversation(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let elsewhere = db::create_conversation(&pool).await.expect("conversation");
    let repo = cloned_repo(&pool, conversation.id).await;
    request_or_load(&pool, conversation.id, &repo, "AGENTS.md", &file("x")).await.expect("ask");
    let shown = only_repo(&pool, conversation.id).await.trust_requests.remove(0);
    assert!(decide_trust(&pool, elsewhere.id, shown.id, &shown.hash, true).await.is_err());
    assert_eq!(db::get_repo_trust(&pool, "github.com/o/r").await.expect("get"), None);
}

#[sqlx::test]
async fn test_a_clone_dropped_midway_is_marked_interrupted(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let repo = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
        .await
        .expect("repo");
    // Finished: left alone.
    CloneGuard::new(&pool, &repo, conversation.id).finish();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let row = db::get_conversation_repo(&pool, repo.id).await.expect("get").expect("exists");
    assert_eq!(row.status, "cloning");
    // Dropped mid-clone: marked failed, so turns stop waiting on it.
    drop(CloneGuard::new(&pool, &repo, conversation.id));
    let marked = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let row = db::get_conversation_repo(&pool, repo.id).await.expect("get").expect("exists");
            if row.status == "failed" {
                return row;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the dropped clone is marked failed");
    assert_eq!(marked.error.as_deref(), Some(CLONE_INTERRUPTED));
    assert!(wait_for_clones(&pool, conversation.id, std::time::Duration::from_millis(100)).await);
}

#[sqlx::test]
async fn test_a_stop_after_the_clone_is_recorded_doesnt_mark_it_interrupted(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let repo = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
        .await
        .expect("repo");
    let guard = CloneGuard::new(&pool, &repo, conversation.id);
    let cloned = ClonedRepo {
        commit: Some("abc".to_string()),
        branch: "main".to_string(),
    };
    record_cloned(&pool, &repo, &cloned, Ok(vec![]), guard).await.expect("record");
    // What follows is cut off here.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let row = db::get_conversation_repo(&pool, repo.id).await.expect("get").expect("exists");
    assert_eq!((row.status.as_str(), row.error.as_deref()), ("ready", None));
}

/// A turn stopped just after the clone was recorded ready, before
/// its guard was disarmed: the guard's "interrupted" doesn't undo
/// it (SME-86).
#[sqlx::test]
async fn test_a_guard_firing_after_ready_leaves_the_clone_ready(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let repo = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
        .await
        .expect("repo");
    let guard = CloneGuard::new(&pool, &repo, conversation.id);
    db::set_repo_ready(&pool, repo.id, repo.attempt, "main", Some("abc"), &[], None)
        .await
        .expect("ready");
    drop(guard);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let row = db::get_conversation_repo(&pool, repo.id).await.expect("get").expect("exists");
    assert_eq!((row.status.as_str(), row.error.as_deref()), ("ready", None));
}

/// A replaced attempt whose clone (or pod start) fails is told it
/// was superseded, not that the repo failed (SME-86 code review).
#[sqlx::test]
async fn test_a_superseded_attempt_that_fails_is_told_so(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let stale = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
        .await
        .expect("repo");
    assert_eq!(fail_attempt(&pool, &stale, "fatal: nope").await, "fatal: nope");
    db::retry_repo_clone(&pool, stale.id, "u", "k", None)
        .await
        .expect("retry")
        .expect("it was failed");
    assert_eq!(fail_attempt(&pool, &stale, "fatal: again").await, CLONE_SUPERSEDED);
    let row = db::get_conversation_repo(&pool, stale.id).await.expect("get").expect("exists");
    assert_eq!(row.status, "cloning");
}

/// A retry that loses the race to another retry of the same failed
/// clone is refused, saying why (SME-86).
#[sqlx::test]
async fn test_a_retry_racing_another_is_refused(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let repo = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
        .await
        .expect("repo");
    db::set_repo_failed(&pool, repo.id, repo.attempt, "fatal: nope").await.expect("fail");
    assert!(record_clone(&pool, conversation.id, "u", None, "k", "r", Some(repo.id))
        .await
        .expect("the first retry")
        .is_some());
    assert!(
        record_clone(&pool, conversation.id, "u", None, "k", "r", Some(repo.id))
            .await
            .expect("the second retry")
            .is_none(),
        "the second retry runs nothing"
    );
}

/// Two fresh clones into the same directory at once (\"Work on a
/// repo\" while the model clones it): the second gets the first's
/// checkout, not a database error (SME-86 code review 2).
#[sqlx::test]
async fn test_a_fresh_clone_that_lost_the_race_gets_the_winners_checkout(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let url = "git@github.com:o/r.git";
    let first = record_clone(&pool, conversation.id, url, None, "github.com/o/r", "r", None)
        .await
        .expect("the first clone")
        .expect("recorded");
    let second = record_clone(&pool, conversation.id, url, None, "github.com/o/r", "r", None)
        .await
        .expect("the second clone isn't a database error");
    assert!(second.is_none(), "the second runs nothing: {second:?}");
    let existing = after_lost_race(&pool, conversation.id, url, None, "r").await.expect("its checkout");
    assert_eq!(existing.id, first.id);
}

/// A request whose retry lost the race gets the other request's
/// checkout when it's the same repo and branch, cloning or done, and
/// the usual refusal otherwise (SME-86 code review).
#[sqlx::test]
async fn test_a_retry_that_lost_the_race_gets_the_winners_checkout(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let url = "git@github.com:o/r.git";
    let repo = db::create_conversation_repo(&pool, conversation.id, url, "github.com/o/r", None, "r")
        .await
        .expect("repo");
    // Still cloning for the winner: this request waits on the same row.
    let cloning = after_lost_race(&pool, conversation.id, url, None, "r").await.expect("cloning");
    assert_eq!((cloning.id, cloning.status.as_str()), (repo.id, "cloning"));
    // The winner finished: the checkout is this request's too.
    db::set_repo_ready(&pool, repo.id, repo.attempt, "main", Some("abc"), &[], None)
        .await
        .expect("ready");
    let ready = after_lost_race(&pool, conversation.id, url, None, "r").await.expect("ready");
    assert_eq!((ready.id, ready.status.as_str()), (repo.id, "ready"));
    // Another repo is there now: refused, as a fresh request would be.
    let other = after_lost_race(&pool, conversation.id, "git@github.com:o/other.git", None, "r")
        .await
        .expect_err("another repo");
    assert!(other.contains("already used by"), "{other}");
}

/// An attempt that a retry has replaced can't record its clone as
/// the repo's, and says so (SME-86).
#[sqlx::test]
async fn test_a_superseded_attempt_doesnt_record_its_clone(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let stale = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
        .await
        .expect("repo");
    db::set_repo_failed(&pool, stale.id, stale.attempt, "fatal: nope").await.expect("fail");
    let current = db::retry_repo_clone(&pool, stale.id, "u", "k", None)
        .await
        .expect("retry")
        .expect("it was failed");
    let cloned = ClonedRepo { commit: Some("abc".to_string()), branch: "main".to_string() };
    let guard = CloneGuard::new(&pool, &stale, conversation.id);
    let refused = record_cloned(&pool, &stale, &cloned, Ok(vec![]), guard)
        .await
        .expect_err("superseded");
    assert!(refused.contains("superseded"), "{refused}");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let row = db::get_conversation_repo(&pool, stale.id).await.expect("get").expect("exists");
    assert_eq!((row.status.as_str(), row.attempt), ("cloning", current.attempt));
}

/// A repo is ready only once its AGENTS.md files are listed, so a
/// turn that waited for the clone sees them (SME-32 code review 10,
/// finding 3).
#[sqlx::test]
async fn test_a_repo_is_ready_with_its_agents_files_already_listed(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let cloned = ClonedRepo { commit: Some("abc".to_string()), branch: "main".to_string() };

    let listed = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "listed").await.expect("repo");
    let guard = CloneGuard::new(&pool, &listed, conversation.id);
    let files = vec!["AGENTS.md".to_string(), "api/AGENTS.md".to_string()];
    record_cloned(&pool, &listed, &cloned, Ok(files.clone()), guard).await.expect("record");
    let row = db::get_conversation_repo(&pool, listed.id).await.expect("get").expect("exists");
    assert_eq!((row.status.as_str(), row.error.as_deref(), row.agents_files), ("ready", None, files));

    let unlisted = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "unlisted").await.expect("repo");
    let guard = CloneGuard::new(&pool, &unlisted, conversation.id);
    record_cloned(&pool, &unlisted, &cloned, Err("exec failed".to_string()), guard).await.expect("record");
    let row = db::get_conversation_repo(&pool, unlisted.id).await.expect("get").expect("exists");
    assert_eq!(row.status, "ready", "the clone itself is good");
    assert_eq!(row.error.as_deref(), Some("Couldn't list its AGENTS.md files: exec failed"));
}

/// No key and no identity: a new pod is left alone (SME-32 code
/// review 8, finding 2). Pod 999999 doesn't exist, and this test
/// process has no cluster: any attempt to write would fail.
#[sqlx::test]
async fn test_a_new_pod_gets_nothing_when_there_is_nothing_to_install(pool: PgPool) {
    install_into_new_pod(&pool, 999_999).await.expect("nothing to do");
}

#[sqlx::test]
async fn test_delete_key_and_save_identity(pool: PgPool) {
    let key = create_key(&pool, "gone").await.expect("create");
    delete_key(&pool, key.id).await.expect("delete");
    assert!(list_keys(&pool).await.expect("list").is_empty());

    let identity = GitIdentity {
        name: "Ada".into(),
        email: "ada@example.com".into(),
    };
    save_identity(&pool, &identity).await.expect("save");
    assert_eq!(db::get_git_identity(&pool).await.expect("get"), identity);
    let bad = GitIdentity {
        name: "Ada\nEvil".into(),
        email: "ada@example.com".into(),
    };
    assert!(save_identity(&pool, &bad).await.is_err());
    assert_eq!(db::get_git_identity(&pool).await.expect("get"), identity);
}
