//! Trust: cards, decisions, what loads, what is unloaded, and the notices.

use super::*;

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

/// A remote trusted without its card being answered (the model
/// loading again once it's trusted, or "Work on a repo") leaves no
/// card behind, and every waiting model hears (SME-32 code review
/// 9, finding 1).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_request_is_decided_only_from_its_own_conversation(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let elsewhere = db::create_conversation(&pool).await.expect("conversation");
    let repo = cloned_repo(&pool, conversation.id).await;
    request_or_load(&pool, conversation.id, &repo, "AGENTS.md", &file("x")).await.expect("ask");
    let shown = only_repo(&pool, conversation.id).await.trust_requests.remove(0);
    assert!(decide_trust(&pool, elsewhere.id, shown.id, &shown.hash, true).await.is_err());
    assert_eq!(db::get_repo_trust(&pool, "github.com/o/r").await.expect("get"), None);
}

/// SME-246: every write of an AGENTS.md trust card ends in
/// `publish_repos`, which tells every tab's sidebar that what waits for
/// the user may have changed: a card waits for the user as a question does.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_publishing_a_conversations_repos_announces_its_waits(pool: PgPool) {
    let id = db::create_conversation(&pool).await.expect("conversation").id;
    let recording = crate::events::AppRecording::start();
    publish_repos(&pool, id).await;
    assert_eq!(recording.events(), vec![crate::events::AppEvent::QuestionsChanged]);
}
