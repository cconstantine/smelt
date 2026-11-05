//! SME-386 F6: a parent is told why a child's turn ended without a report,
//! refused or cut off, since it's the parent who reports to the user (N6).

use super::*;

async fn queued_to(pool: &PgPool, to: i64) -> Vec<String> {
    sqlx::query_scalar::<_, String>("SELECT body FROM agent_messages WHERE to_conversation_id = $1 ORDER BY id")
        .bind(to)
        .fetch_all(pool)
        .await
        .expect("queued rows")
}

/// A child whose every reply is "Here is how to", stopped with `stop_reason`,
/// and the notice its parent gets.
async fn notice_for_a_child_stopped_with(pool: &PgPool, stop_reason: &str) -> (i64, String) {
    db::test_support::start_ids_clear_of_other_runs(pool).await.expect("ids clear of other runs");
    let body = text_reply("Here is how to").replace("end_turn", stop_reason);
    mock_with(pool, move |request| {
        let reply = if agent_of(request).is_some() { body.clone() } else { text_reply("Noted.") };
        Box::pin(async move { reply })
    })
    .await;
    let root = db::create_conversation(pool).await.expect("create conversation").id;
    let child = start_child(pool, root, "Work.").await;
    eventually("the parent is told", || async { !queued_to(pool, root).await.is_empty() }).await;
    let notice = queued_to(pool, root).await.remove(0);
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
    (child, notice)
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refused_childs_notice_says_it_was_refused(pool: PgPool) {
    let (child, notice) = notice_for_a_child_stopped_with(&pool, "refusal").await;
    assert_eq!(
        notice,
        format!(
            "agent #{child}'s turn ended without a report (the model provider's safety system refused its reply); \
             its last message: Here is how to"
        )
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_cut_off_childs_notice_says_it_was_cut_off(pool: PgPool) {
    let (child, notice) = notice_for_a_child_stopped_with(&pool, "max_tokens").await;
    let start = format!("agent #{child}'s turn ended without a report (its reply was cut off at ");
    assert!(notice.starts_with(&start), "{notice}");
    assert!(notice.contains(" tokens, "), "it says the limit and what bound it: {notice}");
    assert!(notice.ends_with("; its last message: Here is how to"), "{notice}");
}

/// A turn ending as usual says nothing more, as `ended.rs` pins whole.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_plain_end_gives_no_reason(pool: PgPool) {
    let (child, notice) = notice_for_a_child_stopped_with(&pool, "end_turn").await;
    assert_eq!(notice, format!("agent #{child}'s turn ended without a report; its last message: Here is how to"));
}

/// How an agent's failing call answers: the stream ends in an error.
const OVERLOADED: &str = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";

/// The failure notices saved in `id`, as their reasons.
async fn failures_in(pool: &PgPool, id: i64) -> Vec<String> {
    transcript(pool, id)
        .await
        .into_iter()
        .filter_map(|(_, text)| crate::api::chat::parse_failure_notice(&text).map(str::to_string))
        .collect()
}

/// SME-386 F7: the red banner is live only, and a child's first turn runs
/// with no tab open on it, so its page has to say so itself.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_childs_failed_first_turn_is_saved_on_its_own_page(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    mock_with(&pool, |request| {
        let reply = if agent_of(request).is_some() { OVERLOADED.to_string() } else { text_reply("Noted.") };
        Box::pin(async move { reply })
    })
    .await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = start_child(&pool, root, "Work.").await;

    eventually("the parent is told", || async { !queued_to(&pool, root).await.is_empty() }).await;
    let failures = failures_in(&pool, child).await;
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("Overloaded"), "{failures:?}");
    let queued = queued_to(&pool, root).await;
    assert!(queued[0].contains("(it failed: ") && queued[0].contains("Overloaded"), "the parent still hears why: {queued:?}");
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
    assert!(failures_in(&pool, root).await.is_empty(), "the failure was the child's");
}

/// A later turn the user didn't send, a wake, fails the same way, and each
/// failure is saved once.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_childs_failed_wake_is_saved_on_its_own_page(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    mock_with(&pool, move |request| {
        let is_agent = agent_of(request).is_some();
        let call = if is_agent { calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) } else { 0 };
        let reply = if is_agent && call > 0 { OVERLOADED.to_string() } else { text_reply("Noted.") };
        Box::pin(async move { reply })
    })
    .await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = start_child(&pool, root, "Work.").await;
    eventually("the first turn tells the parent", || async { !queued_to(&pool, root).await.is_empty() }).await;
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
    assert!(failures_in(&pool, child).await.is_empty(), "its first turn went fine");

    db::queue_notice(&pool, child, root, "agent", "Anything else?").await.expect("a notice");
    crate::turn::notify(&pool, child, vec![crate::turn::Notice::Wake]);

    eventually("the wake's failure is saved", || async { !failures_in(&pool, child).await.is_empty() }).await;
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
    assert_eq!(failures_in(&pool, child).await.len(), 1);

    // Review L6: another wake failing the same way says nothing new.
    let before = queued_to(&pool, root).await.len();
    db::queue_notice(&pool, child, root, "agent", "And now?").await.expect("a notice");
    crate::turn::notify(&pool, child, vec![crate::turn::Notice::Wake]);
    eventually("the wake's failure tells the parent", || async { queued_to(&pool, root).await.len() > before }).await;
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
    assert_eq!(failures_in(&pool, child).await.len(), 1);
}
