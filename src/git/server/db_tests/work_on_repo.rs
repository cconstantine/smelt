//! "Work on a repo": recording it, correcting it, trusting a checkout.

use super::*;

/// "Work on a repo" on a repo the model already cloned still
/// counts as the user picking it (SME-32 code review 7, finding 4).
/// The Clone button's request only records: the sandbox and the
/// clone run after it, so a closed tab can't cut them off (SME-32
/// code review 8, finding 1).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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
