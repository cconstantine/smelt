//! The attention tests' SME-370 section (split from `attention.rs`,
//! SME-377).

use super::*;

// --- An idle-notice turn notes no Finished (SME-370) ---

/// The ticket's reason to exist: a wake whose drain saved an idle notice is
/// smelt telling itself about a command that keeps running, not an answer
/// the user was waiting on — the busy spell ends owing nothing, and the
/// repeated pings never reach the desktop. (Prediction, red on the unfixed
/// code: the `is_empty` fails naming one event, `[(Finished, …)]`. The
/// actual red is recorded beside it in What shipped.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_idle_notice_wake_publishes_nothing(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    untouched_running_command(&pool, conversation.id, "cmd-idle", 601).await;
    start_mock_upstream(&pool, vec![done_body("Still running.")]).await;
    let recording = events::AppRecording::start();

    wake_conversation(&pool, conversation.id).await.expect("wake");

    let seen = published_after_done(&recording, conversation.id).await;
    assert!(seen.is_empty(), "an idle-notice turn notifies nothing, saw {seen:?}");
}

/// The mixed-drain call (SME-370's Trade-offs): a turn whose drain carried a
/// finished command's notice *and* an idle notice — the command ended during
/// that same turn — still notes nothing. The miss is toward quiet, and the
/// command's user got its output in the transcript either way. (Prediction,
/// red before the fix: one Finished event.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_with_a_finished_command_and_an_idle_notice_publishes_nothing(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    finished_and_idle_commands(&pool, conversation.id).await;
    start_mock_upstream(&pool, vec![done_body("Both noted.")]).await;
    let recording = events::AppRecording::start();

    wake_conversation(&pool, conversation.id).await.expect("wake");

    let seen = published_after_done(&recording, conversation.id).await;
    assert!(
        seen.is_empty(),
        "an idle notice in the drain takes the Finished with it, saw {seen:?}"
    );
}

/// What an idle-notice turn still notifies: a question it asked still
/// reaches the user. The guard is the Finished arm's alone — `ask` notes the
/// Question at ask time, whatever else the turn saved. (Cannot go red
/// pre-fix, as predicted: Question already outranks Finished. Honest reach
/// as in the `ask_user` test above — it bites if the guard is over-plumbed,
/// e.g. moved into `note_attention`, where the ask's note would vanish
/// with it.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_question_asked_on_an_idle_notice_turn_publishes_question(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    untouched_running_command(&pool, conversation.id, "cmd-idle", 601).await;
    start_mock_upstream(&pool, vec![ask_body()]).await;
    let recording = events::AppRecording::start();

    wake_conversation(&pool, conversation.id).await.expect("the waking turn asks");

    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Question);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = needs_user_of(&recording, conversation.id);
    assert_eq!(seen.len(), 1, "one Question, nothing alongside, saw {seen:?}");
    assert_eq!(seen[0].0, events::Attention::Question);
}

/// And a failing idle-notice turn still notifies: the error arm notes Failed
/// whatever the turn saved. (Same honest reach as the question test — cannot
/// go red pre-fix — and it bites if the guard is over-plumbed into skipping
/// every note for such a turn.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_failing_idle_notice_wake_publishes_failed(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    untouched_running_command(&pool, conversation.id, "cmd-idle", 601).await;
    start_mock_upstream_failing_n_times(&pool, 100, None).await;
    let recording = events::AppRecording::start();

    wake_conversation(&pool, conversation.id).await.expect_err("the failing call fails the wake");

    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Failed);
}
