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
