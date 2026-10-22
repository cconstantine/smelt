use super::*;

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_ssh_key_round_trips_and_deletes(pool: PgPool) {
    let work = create_ssh_key(&pool, "work", "ssh-ed25519 AAAA work", "PRIVATE-W")
        .await
        .expect("create key");
    let listed = list_ssh_keys(&pool).await.expect("list keys");
    assert_eq!(listed, vec![work.clone()]);
    assert_eq!(listed[0].private_key, "PRIVATE-W");

    delete_ssh_key(&pool, work.id).await.expect("delete key");
    assert!(list_ssh_keys(&pool).await.expect("list keys").is_empty());
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_git_identity_is_empty_until_set_then_updates_in_place(pool: PgPool) {
    assert_eq!(
        get_git_identity(&pool).await.expect("get"),
        crate::git::GitIdentity::default()
    );
    let first = crate::git::GitIdentity {
        name: "Ada".into(),
        email: "ada@example.com".into(),
    };
    set_git_identity(&pool, &first).await.expect("set");
    assert_eq!(get_git_identity(&pool).await.expect("get"), first);

    let second = crate::git::GitIdentity {
        name: "Ada Lovelace".into(),
        email: "ada@lovelace.dev".into(),
    };
    set_git_identity(&pool, &second).await.expect("set again");
    assert_eq!(get_git_identity(&pool).await.expect("get"), second);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_conversation_repos_round_trip_through_a_clone(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("create conversation");
    let repo = create_conversation_repo(
        &pool,
        conversation.id,
        "git@github.com:o/r.git",
        "github.com/o/r",
        Some("dev"),
        "r",
    )
    .await
    .expect("create repo");
    assert_eq!(repo.status, "cloning");
    assert_eq!(repo.branch.as_deref(), Some("dev"));

    set_repo_failed(&pool, repo.id, repo.attempt, "fatal: nope").await.expect("fail");
    let listed = list_conversation_repos(&pool, conversation.id).await.expect("list");
    assert_eq!((listed[0].status.as_str(), listed[0].error.as_deref()), ("failed", Some("fatal: nope")));

    let retried = retry_repo_clone(&pool, repo.id, "ssh://git@github.com/o/r", "github.com/o/r", None)
        .await
        .expect("retry")
        .expect("it was failed");
    assert_eq!((retried.status.as_str(), retried.error.as_deref()), ("cloning", None));
    assert_eq!((retried.url.as_str(), retried.branch.as_deref()), ("ssh://git@github.com/o/r", None));
    set_repo_cloned(&pool, repo.id, "dev", Some("abc123")).await.expect("cloned");
    let listed = list_conversation_repos(&pool, conversation.id).await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].status, "ready");
    assert_eq!(listed[0].error, None);
    assert_eq!(listed[0].checked_out_branch.as_deref(), Some("dev"));
    assert_eq!(listed[0].commit_sha.as_deref(), Some("abc123"));

    // After a restart, a clone still marked cloning was cut off.
    let other = create_conversation(&pool).await.expect("conversation");
    let unfinished = create_conversation_repo(&pool, other.id, "u", "k", None, "x")
        .await
        .expect("repo");
    assert_eq!(fail_unfinished_clones(&pool, "interrupted").await.expect("sweep"), 1);
    let row = get_conversation_repo(&pool, unfinished.id).await.expect("get").expect("exists");
    assert_eq!((row.status.as_str(), row.error.as_deref()), ("failed", Some("interrupted")));
    let untouched = get_conversation_repo(&pool, repo.id).await.expect("get").expect("exists");
    assert_eq!(untouched.status, "ready", "a finished clone is left alone");

    // The checkout's AGENTS.md files.
    set_repo_agents_files(&pool, repo.id, &["AGENTS.md".to_string(), "web/AGENTS.md".to_string()])
        .await
        .expect("agents files");
    let row = get_conversation_repo(&pool, repo.id).await.expect("get").expect("exists");
    assert_eq!(row.agents_files, vec!["AGENTS.md".to_string(), "web/AGENTS.md".to_string()]);

    // Loading an AGENTS.md, and loading it again replaces it.
    let file = |content: &str| InstructionsFile {
        content: content.to_string(),
        file_bytes: content.len() as i64,
        hash: format!("hash-{content}"),
        commit: Some("abc123".to_string()),
    };
    load_instruction(&pool, conversation.id, repo.id, "AGENTS.md", &file("v1")).await.expect("load");
    load_instruction(&pool, conversation.id, repo.id, "AGENTS.md", &file("v2")).await.expect("reload");
    load_instruction(&pool, conversation.id, repo.id, "web/AGENTS.md", &file("web")).await.expect("load nested");
    let loaded = list_loaded_instructions(&pool, conversation.id).await.expect("list loaded");
    let paths: Vec<(&str, &str)> = loaded.iter().map(|l| (l.path.as_str(), l.content.as_str())).collect();
    assert_eq!(paths, vec![("AGENTS.md", "v2"), ("web/AGENTS.md", "web")]);
    assert_eq!(loaded[0].commit_sha.as_deref(), Some("abc123"));

    // A request waiting on trust; asking again replaces it.
    request_instruction(&pool, conversation.id, repo.id, "AGENTS.md", &file("seen")).await.expect("request");
    let request = request_instruction(&pool, conversation.id, repo.id, "AGENTS.md", &file("newer"))
        .await
        .expect("request again");
    let requests = list_instruction_requests(&pool, conversation.id).await.expect("list requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].file(), file("newer"));
    assert_eq!(
        list_instruction_requests_for_remote(&pool, "github.com/o/r").await.expect("by remote"),
        requests
    );
    assert_eq!(get_instruction_request(&pool, request.id).await.expect("get"), Some(request.clone()));
    delete_instruction_request(&pool, request.id).await.expect("delete request");
    assert!(list_instruction_requests(&pool, conversation.id).await.expect("list").is_empty());

    // Trust decisions, remembered per remote either way.
    assert_eq!(get_repo_trust(&pool, "github.com/o/r").await.expect("get"), None);
    set_repo_trust(&pool, "github.com/o/r", false).await.expect("decline");
    set_repo_trust(&pool, "github.com/o/r", true).await.expect("trust");
    assert_eq!(get_repo_trust(&pool, "github.com/o/r").await.expect("get"), Some(true));
    assert_eq!(list_repo_trust(&pool).await.expect("list").len(), 1);
    delete_repo_trust(&pool, "github.com/o/r").await.expect("forget");
    assert_eq!(get_repo_trust(&pool, "github.com/o/r").await.expect("get"), None);

    // One checkout per directory.
    assert!(
        create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
            .await
            .is_err()
    );
    // Deleting the conversation deletes its repos.
    delete_conversation(&pool, conversation.id).await.expect("delete conversation");
    assert!(list_conversation_repos(&pool, conversation.id).await.expect("list").is_empty());
}

/// Only a failed clone is retried, and only the current attempt can
/// finish a clone: a stale attempt's late "ready" or "failed" changes
/// nothing (SME-86).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_only_the_current_attempt_finishes_a_clone(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let repo = create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
        .await
        .expect("create repo");
    assert_eq!(repo.attempt, 1);
    let status = |pool: PgPool| async move {
        let row = get_conversation_repo(&pool, repo.id).await.expect("get").expect("exists");
        (row.status, row.attempt)
    };

    // A clone that's still running isn't retried.
    assert!(retry_repo_clone(&pool, repo.id, "u", "k", None).await.expect("retry").is_none());
    assert_eq!(status(pool.clone()).await, ("cloning".to_string(), 1));

    // Attempt 1 finishes; its guard's late "failed" changes nothing.
    assert!(set_repo_ready(&pool, repo.id, 1, "main", None, &[], &[], None).await.expect("ready"));
    assert!(!set_repo_failed(&pool, repo.id, 1, "interrupted").await.expect("late failed"));
    assert_eq!(status(pool.clone()).await, ("ready".to_string(), 1));
    // A ready clone isn't retried either.
    assert!(retry_repo_clone(&pool, repo.id, "u", "k", None).await.expect("retry").is_none());

    // Another clone's failed attempt 1 is retried as attempt 2;
    // attempt 1's late writes don't touch it.
    let failed = create_conversation_repo(&pool, conversation.id, "u", "k", None, "f")
        .await
        .expect("create repo");
    assert!(set_repo_failed(&pool, failed.id, 1, "fatal: nope").await.expect("fail"));
    let retried = retry_repo_clone(&pool, failed.id, "u", "k", None)
        .await
        .expect("retry")
        .expect("it was failed");
    assert_eq!((retried.status.as_str(), retried.attempt), ("cloning", 2));
    assert!(!set_repo_failed(&pool, failed.id, 1, "interrupted").await.expect("stale failed"));
    assert!(!set_repo_ready(&pool, failed.id, 1, "main", None, &[], &[], None).await.expect("stale ready"));
    let row = get_conversation_repo(&pool, failed.id).await.expect("get").expect("exists");
    assert_eq!((row.status.as_str(), row.attempt), ("cloning", 2));
    assert!(set_repo_ready(&pool, failed.id, 2, "main", None, &[], &[], None).await.expect("ready"));
    let row = get_conversation_repo(&pool, failed.id).await.expect("get").expect("exists");
    assert_eq!(row.status, "ready");
}

/// Two retries of the same failed clone at once: exactly one runs.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_two_retries_of_a_failed_clone_one_wins(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let repo = create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
        .await
        .expect("create repo");
    assert!(set_repo_failed(&pool, repo.id, 1, "fatal: nope").await.expect("fail"));
    let (a, b) = tokio::join!(
        retry_repo_clone(&pool, repo.id, "u", "k", None),
        retry_repo_clone(&pool, repo.id, "u", "k", None),
    );
    let won = [a.expect("retry a"), b.expect("retry b")].into_iter().flatten().count();
    assert_eq!(won, 1, "exactly one retry runs");
}

/// SME-235: a finished clone records its persona files with its
/// AGENTS.md files, in the one update, and a retry starts them empty.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_clone_records_its_persona_files_and_a_retry_clears_them(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let repo = create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
        .await
        .expect("create repo");
    assert!(repo.persona_files.is_empty(), "{repo:?}");
    let agents = vec!["AGENTS.md".to_string()];
    let personas = vec![".agents/personas/developer.md".to_string(), ".agents/personas/reviewer.md".to_string()];
    assert!(set_repo_ready(&pool, repo.id, 1, "main", None, &agents, &personas, None).await.expect("ready"));
    let row = get_conversation_repo(&pool, repo.id).await.expect("get").expect("exists");
    assert_eq!((row.agents_files, row.persona_files), (agents, personas.clone()));

    // A failed row still holding some (written by hand: no path leaves
    // one there today) is retried with none.
    let failed = create_conversation_repo(&pool, conversation.id, "u", "k", None, "f")
        .await
        .expect("create repo");
    assert!(set_repo_failed(&pool, failed.id, 1, "fatal: nope").await.expect("fail"));
    sqlx::query("UPDATE conversation_repos SET persona_files = $2 WHERE id = $1")
        .bind(failed.id)
        .bind(&personas)
        .execute(&pool)
        .await
        .expect("set persona files");
    let retried = retry_repo_clone(&pool, failed.id, "u", "k", None)
        .await
        .expect("retry")
        .expect("it was failed");
    assert!(retried.persona_files.is_empty(), "{retried:?}");
}
