use super::*;

/// SME-79: `SANDBOX_IDLE_NOTICE_MINUTES` is a whole number of minutes. `0`
/// is one of them — it's how the deployment turns the whole check off — and
/// the default is 5.
#[test]
fn test_idle_notice_minutes_is_a_whole_number_of_minutes() {
    assert_eq!(idle_notice_secs_from(Some("1".to_string())), 60);
    assert_eq!(idle_notice_secs_from(Some("10".to_string())), 600);
    assert_eq!(idle_notice_secs_from(Some("0".to_string())), 0, "0 is the check turned off, not a fallback");
}

#[test]
fn test_idle_notice_minutes_falls_back_to_five_minutes_with_a_warning() {
    let default = 300;
    assert_eq!(idle_notice_secs_from(None), default);
    assert_eq!(idle_notice_secs_from(Some(String::new())), default, "an empty setting is no setting");
    for bad in ["five", "-3", "2.5", " 5 "] {
        assert_eq!(idle_notice_secs_from(Some(bad.to_string())), default, "{bad:?} is not minutes");
    }
}

/// SME-79 review 1: a whole number of minutes is only a sane interval up
/// to a day. Past it, `minutes * 60` can overflow — a debug build panics
/// inside the tool call, a release build wraps, and the wrapped value
/// lands either on the `CHECK (idle_notice_secs >= 0)` (every
/// `run_terminal_command` failing with an opaque constraint error) or on a
/// silently different interval. Anything past the ceiling is treated like
/// the other bad values: warn and use the default. The values below are
/// the ones from the review, each of which escaped the documented fallback
/// in its own way.
#[test]
fn test_idle_notice_minutes_past_the_ceiling_falls_back_like_a_bad_value() {
    let default = 300;
    // The ceiling itself is still an interval, and the multiply at this
    // size can't overflow.
    assert_eq!(idle_notice_secs_from(Some("1440".to_string())), 86_400);
    for too_long in [
        "1441", // one minute past the ceiling
        "40000000000", // seconds past i32: used to mean i32::MAX, silently
        "153722867280912931", // * 60 overflows i64: a debug-build panic
        "307445734561825860", // wraps to -16 s in release: a CHECK violation on every command
        "307445734561825861", // wraps to +44 s in release: notices every 44 seconds, no warning
        "99999999999999999999999999999999999999", // past i64: doesn't parse at all
    ] {
        assert_eq!(idle_notice_secs_from(Some(too_long.to_string())), default, "{too_long} is past a day, not an interval");
    }
}

/// Move a command's clock back `secs` from its start — nothing has touched
/// these, so that's where they count from.
async fn backdate_created(pool: &sqlx::PgPool, command_id: &str, secs: i64) {
    sqlx::query(
        "UPDATE terminal_commands SET created_at = now() - make_interval(secs => $1) WHERE command_id = $2",
    )
    .bind(secs as f64)
    .bind(command_id)
    .execute(pool)
    .await
    .expect("backdate created_at");
}

/// A conversation holding `command_ids`, each running on the 300-second
/// interval and untouched for ten minutes.
async fn idle_conversation(pool: &sqlx::PgPool, command_ids: &[&str]) -> i64 {
    let conversation = db::create_conversation(pool).await.expect("conversation");
    let pod = db::create_sandbox_pod(pool, conversation.id).await.expect("pod");
    let terminal = db::create_sandbox_terminal(pool, pod.id).await.expect("terminal");
    for id in command_ids {
        db::create_terminal_command(pool, conversation.id, terminal.id, id, "sleep 100", 300)
            .await
            .expect("command");
        backdate_created(pool, id, 600).await;
    }
    conversation.id
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_idle_watch_pass_wakes_only_the_conversations_with_due_commands(pool: sqlx::PgPool) {
    let _guard = crate::providers::test_support::lock_turn_tests();
    // Wakes publish on the conversation id's channel, and the drain saves
    // into the conversation: keep the ids clear of other runs' (SME-135).
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let idle = idle_conversation(&pool, &["idle-w1", "idle-w2"]).await;
    // A conversation whose command asked for no notice is never a due one.
    let quiet = db::create_conversation(&pool).await.expect("conversation");
    let quiet_pod = db::create_sandbox_pod(&pool, quiet.id).await.expect("pod");
    let quiet_terminal = db::create_sandbox_terminal(&pool, quiet_pod.id).await.expect("terminal");
    db::create_terminal_command(&pool, quiet.id, quiet_terminal.id, "idle-w-off", "sleep 100", 0)
        .await
        .expect("command");
    backdate_created(&pool, "idle-w-off", 600).await;

    let woken = notice_idle_commands_once(&pool).await;
    assert_eq!(
        woken,
        vec![idle],
        "one wake for the conversation, not one per command; the never-notice command's conversation is not woken"
    );
}

/// The messages a conversation has been sent, waiting for the wake's drain
/// to have saved `want` of them. The wake's API call fails afterwards with
/// no model configured, which is fine — the drain commits first, as
/// `test_a_wake_with_no_model_still_saves_the_notice` shows for finishes.
async fn notices(pool: &sqlx::PgPool, conversation_id: i64, want: usize) -> Vec<String> {
    for _ in 0..100 {
        let texts: Vec<String> = db::list_messages(pool, conversation_id)
            .await
            .expect("list messages")
            .iter()
            .filter(|m| m.content.contains("is still running"))
            .map(|m| m.content.clone())
            .collect();
        if texts.len() >= want {
            return texts;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("only {want} idle notices had arrived");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_idle_watch_wake_saves_the_notices_and_repeats_the_interval(pool: sqlx::PgPool) {
    let _guard = crate::providers::test_support::lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = idle_conversation(&pool, &["idle-w3", "idle-w4"]).await;

    notice_idle_commands_once(&pool).await;
    let texts = notices(&pool, conversation_id, 2).await;
    assert_eq!(texts.len(), 2, "one notice per due command: {texts:?}");
    for text in &texts {
        // The numbers the model decides with, and the reassurance that
        // nothing is being stopped.
        assert!(text.contains("Nothing stopped it and smelt won't stop it"), "{text}");
        assert!(text.contains("set_idle_notice"), "the way to stop the notices: {text}");
    }

    // Each notice restarted its command's clock, so the next pass finds
    // nothing.
    assert!(notice_idle_commands_once(&pool).await.is_empty(), "the notice just sent restarted the clock");

    // And when that interval passes, it's due again: no cap on notices, only
    // a touch ends them (SME-79).
    for id in ["idle-w3", "idle-w4"] {
        sqlx::query("UPDATE terminal_commands SET idle_notice_at = now() - make_interval(secs => 301) WHERE command_id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .expect("backdate idle_notice_at");
    }
    assert_eq!(notice_idle_commands_once(&pool).await, vec![conversation_id]);
    assert_eq!(notices(&pool, conversation_id, 4).await.len(), 4, "a second notice for each command");
}

/// SME-79 review 2: a conversation the user has stopped is left out of the
/// pass. Its wake is a no-op there — `wake_conversation` returns early while
/// the conversation is paused — and saves and marks nothing, so the row stayed
/// due and every pass woke it again for as long as it stayed stopped: a task
/// spawned only to discover it was paused, and an info line claiming a wake
/// that never happened, once a minute. The notice is held, not lost — the
/// user's next message drains it with that moment's numbers, which
/// `turn::tests::wake::test_a_stopped_conversation_holds_the_idle_notice_for_the_next_message`
/// pins.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stopped_conversation_is_left_out_of_the_idle_pass(pool: sqlx::PgPool) {
    let _guard = crate::providers::test_support::lock_turn_tests();
    // A conversation this test pauses, so an id no other run's conversation
    // shares (SME-135).
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let stopped = idle_conversation(&pool, &["idle-paused-1"]).await;
    let running = idle_conversation(&pool, &["idle-running-1"]).await;

    crate::turn::stop_turn_now(stopped);
    assert!(crate::turn::is_paused(stopped), "the user has that conversation stopped");

    assert_eq!(
        notice_idle_commands_once(&pool).await,
        vec![running],
        "the stopped conversation is skipped; the other is woken as usual"
    );
    let command = db::get_terminal_command(&pool, "idle-paused-1")
        .await
        .expect("query")
        .expect("the command row");
    assert!(command.idle_notice_at.is_none(), "its notice is still owed, not spent on a wake that didn't happen");

    crate::turn::forget_conversation_lock(stopped);
}
