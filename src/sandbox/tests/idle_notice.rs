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

#[sqlx::test]
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

#[sqlx::test]
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
