//! The `/git` settings page's server functions (SME-32): SSH keys and the
//! commit identity. Each is a thin wrapper over `crate::git`, which the
//! model's tools share.

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

use crate::git::{GitIdentity, RepoSummary, RepoTrustSummary, SshKeySummary};
#[cfg(feature = "server")]
use crate::{db, git};

/// Everything the settings page shows.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct GitSettings {
    pub identity: GitIdentity,
    pub keys: Vec<SshKeySummary>,
    pub trust: Vec<RepoTrustSummary>,
}

#[get("/api/git")]
pub async fn get_git_settings() -> ServerFnResult<GitSettings> {
    let pool = db::get();
    Ok(GitSettings {
        identity: db::get_git_identity(pool).await.map_err(ServerFnError::new)?,
        keys: git::list_keys(pool).await.map_err(ServerFnError::new)?,
        trust: db::list_repo_trust(pool)
            .await
            .map_err(ServerFnError::new)?
            .into_iter()
            .map(|t| RepoTrustSummary {
                remote: t.remote_key,
                trusted: t.trusted,
            })
            .collect(),
    })
}

/// Forgets a trust decision: the user is asked again the next time the
/// remote is cloned. What's loaded stays until reloaded.
#[post("/api/git/trust/forget")]
pub async fn forget_repo_trust(remote: String) -> ServerFnResult<()> {
    db::delete_repo_trust(db::get(), &remote)
        .await
        .map_err(ServerFnError::new)
}

/// The trust card's answer about request `request_id`, with the hash of
/// the file the card showed. The model is told, and woken to carry on,
/// even if the tab closes meanwhile (SME-225).
#[post("/api/conversations/{id}/instruction-requests/{request_id}")]
pub async fn decide_repo_trust(id: i64, request_id: i64, shown_hash: String, trusted: bool) -> ServerFnResult<()> {
    decide_and_notify(db::get(), id, request_id, shown_hash, trusted)
        .await
        .map_err(ServerFnError::new)
}

/// `decide_repo_trust`'s work, in a task of its own (SME-225): a request
/// dropped mid-way (the tab closes, the connection resets) stops only the
/// wait for it. The decision still commits, its tabs are still told, and
/// so is every waiting model. A decision that fails with nobody waiting
/// needs nothing more: it rolled back, and the card still shows.
#[cfg(feature = "server")]
async fn decide_and_notify(
    pool: &sqlx::PgPool,
    id: i64,
    request_id: i64,
    shown_hash: String,
    trusted: bool,
) -> Result<(), String> {
    let pool = pool.clone();
    tokio::spawn(async move {
        let notices = git::decide_trust(&pool, id, request_id, &shown_hash, trusted).await?;
        deliver(&pool, notices);
        Ok(())
    })
    .await
    .map_err(|e| format!("The decision didn't finish: {e}"))?
}

/// Delivers each notice in the background: each waits for its
/// conversation's turn to end.
#[cfg(feature = "server")]
fn deliver(pool: &sqlx::PgPool, notices: Vec<(i64, String)>) {
    for (conversation_id, notice) in notices {
        crate::turn::notify(pool, conversation_id, vec![crate::turn::Notice::Deliver(notice)]);
    }
}

/// The conversation's repos, for the sandbox panel.
#[get("/api/conversations/{id}/repos")]
pub async fn list_conversation_repos(id: i64) -> ServerFnResult<Vec<RepoSummary>> {
    git::list_repos(db::get(), id).await.map_err(ServerFnError::new)
}

/// "Work on a repo": records the repo and returns at once; the sandbox
/// start and the clone run in the background. All of it runs apart from
/// the request, so a closed tab can't cut any of it off (SME-225). The
/// panel follows them through `ReposUpdate`.
#[post("/api/conversations/{id}/repos")]
pub async fn attach_repo(id: i64, url: String, branch: String, dir: String) -> ServerFnResult<RepoSummary> {
    attach_and_answer(db::get(), id, url, branch, dir)
        .await
        .map_err(ServerFnError::new)
}

/// `attach_repo`'s work, all in a task of its own (SME-225): a request
/// dropped mid-way (the tab closes) stops only the wait for the answer.
/// The trust and the repo are still recorded, the remote's waiting models
/// still told, and the sandbox and the clone still go ahead.
#[cfg(feature = "server")]
async fn attach_and_answer(
    pool: &sqlx::PgPool,
    id: i64,
    url: String,
    branch: String,
    dir: String,
) -> Result<RepoSummary, String> {
    let pool = pool.clone();
    let (answer, answered) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let branch = Some(branch.trim()).filter(|b| !b.is_empty());
        let dir = Some(dir.trim()).filter(|d| !d.is_empty());
        let (shown, pending) = match git::start_attach(&pool, id, url.trim(), branch, dir).await {
            Ok((shown, pending, notices)) => {
                deliver(&pool, notices);
                (shown, pending)
            }
            // A failure after the trust committed still owes the models
            // whose cards it dropped their notices (SME-224 review 1).
            Err(failed) => {
                deliver(&pool, failed.notices);
                return hand_back_attach(&pool, id, answer, Err(failed.error)).await;
            }
        };
        let nothing_to_clone = pending.is_none();
        hand_back_attach(&pool, id, answer, Ok(shown)).await;
        if let Err(e) = git::finish_attach(&pool, id, pending).await {
            tracing::warn!(conversation_id = id, error = %e, "\"Work on a repo\" failed");
            report_attach_failure(&pool, id, nothing_to_clone, &e).await;
        }
    });
    let (started, ack) = answered
        .await
        .map_err(|_| "\"Work on a repo\" stopped before it answered.".to_string())?;
    let _ = ack.send(());
    started
}

/// The attach's start as handed back to its request, with the sender the
/// request acknowledges reading it on.
#[cfg(feature = "server")]
type AttachAnswer = (Result<RepoSummary, String>, tokio::sync::oneshot::Sender<()>);

/// Hands the attach's start back to the request waiting for it. A
/// failure nobody read (the tab closed, before the send or after it) is
/// shown in the conversation instead, as a failure after the answer is
/// (SME-91); a conversation deleted meanwhile is left with no turn state
/// (SME-219).
#[cfg(feature = "server")]
async fn hand_back_attach(
    pool: &sqlx::PgPool,
    conversation_id: i64,
    answer: tokio::sync::oneshot::Sender<AttachAnswer>,
    started: Result<RepoSummary, String>,
) {
    let error = started.as_ref().err().cloned();
    // Seen only once the request says it read it: a request dropped
    // between the send and its read (the tab closed) never saw it
    // (SME-224 review 1).
    let (ack, acked) = tokio::sync::oneshot::channel();
    let seen = answer.send((started, ack)).is_ok() && acked.await.is_ok();
    if let (Some(e), false) = (error, seen) {
        tracing::warn!(conversation_id, error = %e, "\"Work on a repo\" failed with nobody waiting");
        crate::turn::show_error_outside_turn(pool, conversation_id, format!("\"Work on a repo\" failed. {e}")).await;
    }
}

/// A failed "Work on a repo", shown in the conversation when the repo was
/// already checked out: there's no repo row to mark failed then, and the
/// failure was only logged (SME-91). With a clone, the repo's row says it.
/// A conversation deleted meanwhile is left with no turn state (SME-219).
#[cfg(feature = "server")]
async fn report_attach_failure(pool: &sqlx::PgPool, conversation_id: i64, nothing_to_clone: bool, error: &str) {
    if nothing_to_clone {
        crate::turn::show_error_outside_turn(
            pool,
            conversation_id,
            // `error` says what failed ("Couldn't start the sandbox: …").
            format!("\"Work on a repo\" failed. {error}"),
        )
        .await;
    }
}

#[post("/api/git/identity")]
pub async fn save_git_identity(identity: GitIdentity) -> ServerFnResult<()> {
    git::save_identity(db::get(), &identity)
        .await
        .map_err(ServerFnError::new)
}

#[post("/api/git/keys")]
pub async fn generate_ssh_key(name: String) -> ServerFnResult<SshKeySummary> {
    git::create_key(db::get(), name.trim())
        .await
        .map_err(ServerFnError::new)
}

#[post("/api/git/keys/import")]
pub async fn import_ssh_key(name: String, private_key: String) -> ServerFnResult<SshKeySummary> {
    git::import_key_named(db::get(), name.trim(), &private_key)
        .await
        .map_err(ServerFnError::new)
}

#[delete("/api/git/keys/{id}")]
pub async fn delete_ssh_key(id: i64) -> ServerFnResult<()> {
    git::delete_key(db::get(), id)
        .await
        .map_err(ServerFnError::new)
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

    /// SME-91: when the repo was already checked out there's no repo row
    /// to mark failed, so a sandbox that couldn't start was only logged.
    /// It shows in the conversation instead; with a clone, the repo's own
    /// row says it (the panel), so it isn't shown twice. SME-219: one
    /// deleted while its sandbox was starting is left with no turn state.
    #[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
    async fn test_a_failed_attach_with_nothing_to_clone_shows_in_the_conversation(pool: sqlx::PgPool) {
        let _guard = crate::providers::test_support::lock_turn_tests();
        db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
        let shown = db::create_conversation(&pool).await.expect("create conversation").id;
        let mut rx = crate::events::subscribe(shown);
        // What `finish_attach` fails with when there's nothing to clone:
        // `ensure_sandbox`'s error, which says what failed already.
        report_attach_failure(&pool, shown, true, "Couldn't start the sandbox: the sandbox didn't start").await;
        let error = crate::turn::last_turn_error(shown).expect("the error is kept for a reload");
        assert!(error.contains("the sandbox didn't start"), "got: {error}");
        assert_eq!(
            error.to_lowercase().matches("couldn't start the sandbox").count(),
            1,
            "says it once: {error}"
        );
        match rx.try_recv() {
            Ok(crate::events::ConversationEvent::TurnError { message }) => {
                assert!(message.contains("the sandbox didn't start"), "got: {message}")
            }
            other => panic!("no TurnError published: {other:?}"),
        }

        let with_clone = db::create_conversation(&pool).await.expect("create conversation").id;
        report_attach_failure(&pool, with_clone, false, "the sandbox didn't start").await;
        assert_eq!(crate::turn::last_turn_error(with_clone), None);

        let gone = db::create_conversation(&pool).await.expect("create conversation").id;
        db::delete_conversation(&pool, gone).await.expect("delete");
        crate::turn::forget_conversation_lock(gone);
        report_attach_failure(&pool, gone, true, "Couldn't start the sandbox: conversation not found").await;
        assert_eq!(crate::turn::last_turn_error(gone), None, "a deleted conversation kept the error");

        // Not left for the next test sharing this process: a live
        // conversation's error stays until its delete forgets it.
        crate::turn::forget_conversation_lock(shown);
    }

    /// Polls `check` until it holds, for up to 10 s.
    async fn eventually<F, Fut>(what: &str, mut check: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !check().await {
            assert!(std::time::Instant::now() < deadline, "{what}: not within 10 s");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Whether a message in `conversation_id` says `text`.
    async fn told(pool: &sqlx::PgPool, conversation_id: i64, text: &str) -> bool {
        db::list_messages(pool, conversation_id)
            .await
            .expect("messages")
            .iter()
            .any(|m| m.content.contains(text))
    }

    async fn trusted(pool: &sqlx::PgPool) -> bool {
        db::get_repo_trust(pool, "github.com/o/r").await.expect("trust") == Some(true)
    }

    /// SME-225. Interleaving: Trust on A's card is held at its load, its
    /// transaction open, and the request is dropped there, as when the
    /// tab closes or the connection resets. The decision still commits,
    /// and A's model is still told. (Nothing between the commit and the
    /// notices can be held, so the drop comes before the commit; the
    /// decision running apart from the request covers both.)
    #[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
    async fn test_a_trust_decision_whose_request_is_dropped_still_commits_and_tells_the_model(pool: sqlx::PgPool) {
        let _race = git::test_support::ONE_AT_A_TIME.lock().await;
        let _turns = crate::providers::test_support::lock_turn_tests();
        db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
        let a = db::create_conversation(&pool).await.expect("conversation").id;
        let repo = git::test_support::cloned_repo(&pool, a).await;
        git::request_or_load(&pool, a, &repo, "AGENTS.md", &git::test_support::file("x")).await.expect("ask");
        let card = git::list_repos(&pool, a).await.expect("repos").remove(0).trust_requests.remove(0);
        // Paused, so the notice is saved for the model rather than run.
        crate::turn::stop_turn_now(a);

        let holding = git::test_support::hold_row(&pool, "loaded_instructions", a, repo.id).await;
        let request = tokio::spawn({
            let pool = pool.clone();
            async move { decide_and_notify(&pool, a, card.id, card.hash, true).await }
        });
        assert!(git::test_support::waiters_or_finished(&pool, 1, &request).await, "Trust never reached its load");
        request.abort();
        let _ = request.await;
        holding.rollback().await.expect("release the load");

        let p = &pool;
        eventually("the decision commits", || async move { trusted(p).await }).await;
        eventually("A's model is told", || told(p, a, "The user trusts")).await;
        crate::turn::forget_conversation_lock(a);
    }

    /// SME-225 on "Work on a repo" (SME-224's attach). Interleaving: B's
    /// load is held at its card's insert, holding the remote's trust
    /// lock; A's "Work on a repo" waits for it, and its request is dropped
    /// there. The attach still trusts the remote, drops B's card and
    /// tells B.
    #[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
    async fn test_work_on_a_repo_whose_request_is_dropped_still_drops_the_cards_and_tells_the_models(pool: sqlx::PgPool) {
        let _race = git::test_support::ONE_AT_A_TIME.lock().await;
        let _turns = crate::providers::test_support::lock_turn_tests();
        db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
        let a = db::create_conversation(&pool).await.expect("conversation A").id;
        let b = db::create_conversation(&pool).await.expect("conversation B").id;
        // A has the repo checked out and a sandbox: nothing to clone or start.
        git::test_support::cloned_repo(&pool, a).await;
        db::create_sandbox_pod(&pool, a).await.expect("pod row");
        let repo_b = git::test_support::cloned_repo(&pool, b).await;
        crate::turn::stop_turn_now(b);

        let holding = git::test_support::hold_row(&pool, "instruction_requests", b, repo_b.id).await;
        let load = tokio::spawn({
            let pool = pool.clone();
            async move { git::request_or_load(&pool, b, &repo_b, "AGENTS.md", &git::test_support::file("b")).await }
        });
        assert!(git::test_support::waiters_or_finished(&pool, 1, &load).await, "the load never reached its card");
        let request = tokio::spawn({
            let pool = pool.clone();
            async move { attach_and_answer(&pool, a, "https://github.com/o/r".into(), String::new(), String::new()).await }
        });
        assert!(
            git::test_support::waiters_or_finished(&pool, 2, &request).await,
            "the attach never waited for the load"
        );
        request.abort();
        let _ = request.await;
        holding.rollback().await.expect("release the card");

        let loaded = tokio::time::timeout(std::time::Duration::from_secs(10), load)
            .await
            .expect("the load finishes")
            .expect("the load's task");
        assert_eq!(loaded.expect("the load"), git::LoadOutcome::AwaitingTrust);
        let p = &pool;
        eventually("the attach trusts the remote and drops B's card", || async move {
            trusted(p).await && git::list_repos(p, b).await.expect("repos")[0].trust_requests.is_empty()
        })
        .await;
        eventually("B's model is told", || told(p, b, "load_instructions again")).await;
        crate::turn::forget_conversation_lock(a);
        crate::turn::forget_conversation_lock(b);
    }

    /// SME-224 review 1, finding 1. Interleaving: another request's repo
    /// at A's directory, for another remote, is held uncommitted while
    /// A's "Work on a repo" trusts the remote and drops B's card, so A's
    /// record of the clone waits for it; it commits, and A's attach loses
    /// the race and fails. The trust stands, so B's model is still told.
    #[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
    async fn test_work_on_a_repo_that_fails_after_trusting_still_tells_the_models(pool: sqlx::PgPool) {
        let _race = git::test_support::ONE_AT_A_TIME.lock().await;
        let _turns = crate::providers::test_support::lock_turn_tests();
        db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
        let a = db::create_conversation(&pool).await.expect("conversation A").id;
        let b = db::create_conversation(&pool).await.expect("conversation B").id;
        let repo_b = git::test_support::cloned_repo(&pool, b).await;
        db::request_instruction(&pool, b, repo_b.id, "AGENTS.md", &git::test_support::file("b"))
            .await
            .expect("a card in B");
        crate::turn::stop_turn_now(b);

        let mut holding = pool.begin().await.expect("begin the holding transaction");
        sqlx::query(
            "INSERT INTO conversation_repos (conversation_id, url, remote_key, dir, status)
             VALUES ($1, 'https://github.com/x/r', 'github.com/x/r', 'r', 'cloning')",
        )
        .bind(a)
        .execute(&mut *holding)
        .await
        .expect("hold A's directory, uncommitted");
        let attach = tokio::spawn({
            let pool = pool.clone();
            async move { attach_and_answer(&pool, a, "https://github.com/o/r".into(), String::new(), String::new()).await }
        });
        let attach_waited = git::test_support::waiters_or_finished(&pool, 1, &attach).await;
        holding.commit().await.expect("the other request's repo");

        let attached = tokio::time::timeout(std::time::Duration::from_secs(10), attach)
            .await
            .expect("the attach answers")
            .expect("the attach's task");
        assert!(attach_waited, "the attach never waited at A's directory");
        let error = attached.expect_err("the attach loses the race");
        assert!(error.contains("already used by"), "got: {error}");
        assert!(trusted(&pool).await, "the remote is trusted");
        assert!(git::list_repos(&pool, b).await.expect("repos")[0].trust_requests.is_empty(), "B's card is dropped");
        let p = &pool;
        eventually("B's model is told", || told(p, b, "load_instructions again")).await;
        crate::turn::forget_conversation_lock(a);
        crate::turn::forget_conversation_lock(b);
    }

    /// SME-225: "Work on a repo" failing once nobody waits for its answer
    /// (the tab closed) shows the failure in the conversation, as a
    /// failure after the answer does (SME-91). With someone waiting, it's
    /// theirs to show, and the conversation keeps no error.
    #[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
    async fn test_an_attach_failure_nobody_waits_for_shows_in_the_conversation(pool: sqlx::PgPool) {
        let _guard = crate::providers::test_support::lock_turn_tests();
        db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
        let unanswered = db::create_conversation(&pool).await.expect("conversation").id;
        let (answer, answered) = tokio::sync::oneshot::channel();
        drop(answered);
        hand_back_attach(&pool, unanswered, answer, Err("Couldn't save the repo: boom".to_string())).await;
        let error = crate::turn::last_turn_error(unanswered).expect("the error is kept for a reload");
        assert!(error.contains("\"Work on a repo\" failed.") && error.contains("boom"), "got: {error}");

        let waited_for = db::create_conversation(&pool).await.expect("conversation").id;
        let (answer, answered) = tokio::sync::oneshot::channel();
        let handing_back = tokio::spawn({
            let pool = pool.clone();
            async move { hand_back_attach(&pool, waited_for, answer, Err("boom".to_string())).await }
        });
        let (started, ack) = answered.await.expect("the hand-back answers");
        assert!(matches!(started, Err(e) if e == "boom"), "the waiting request gets the error");
        ack.send(()).expect("the hand-back waits for the read");
        tokio::time::timeout(std::time::Duration::from_secs(10), handing_back)
            .await
            .expect("the hand-back finishes")
            .expect("the hand-back's task");
        assert_eq!(crate::turn::last_turn_error(waited_for), None);

        crate::turn::forget_conversation_lock(unanswered);
    }

    /// SME-224 review 1, finding 3. Interleaving: the failure reaches the
    /// request's channel, and the request is dropped before it reads it
    /// (the tab closed between the send and the request's next poll).
    /// Nobody saw it, so it's shown in the conversation.
    #[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
    async fn test_an_attach_failure_sent_but_never_read_shows_in_the_conversation(pool: sqlx::PgPool) {
        let _guard = crate::providers::test_support::lock_turn_tests();
        db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
        let unread = db::create_conversation(&pool).await.expect("conversation").id;
        let (answer, answered) = tokio::sync::oneshot::channel();
        let handing_back = hand_back_attach(&pool, unread, answer, Err("Couldn't save the repo: boom".to_string()));
        tokio::pin!(handing_back);
        // Sent, and waiting to hear it was read.
        let waited = tokio::time::timeout(std::time::Duration::from_millis(200), &mut handing_back).await.is_err();
        drop(answered);
        if waited {
            tokio::time::timeout(std::time::Duration::from_secs(10), handing_back)
                .await
                .expect("the hand-back finishes");
        }
        let error = crate::turn::last_turn_error(unread).expect("the error is kept for a reload");
        assert!(error.contains("\"Work on a repo\" failed.") && error.contains("boom"), "got: {error}");
        assert!(waited, "the hand-back didn't wait to hear the request read the failure");

        crate::turn::forget_conversation_lock(unread);
    }
}
