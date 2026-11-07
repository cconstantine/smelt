//! Turns on a Claude Code provider: the Lows SME-431 carried in from
//! PR #285's reviews (SME-430).

use super::*;

/// The cut-off notices saved in `conversation_id`, parsed.
async fn cut_off_notices(pool: &PgPool, conversation_id: i64) -> Vec<(u32, crate::api::chat::ReplyLimit)> {
    saved(pool, conversation_id)
        .await
        .into_iter()
        .flat_map(|(_, blocks)| blocks)
        .filter_map(|block| match block {
            anthropic::ContentBlock::Text { text } => crate::api::chat::parse_cut_off_notice(&text).map(|(tokens, limit, _)| (tokens, limit)),
            _ => None,
        })
        .collect()
}

/// L19: a kept run's reply cut off at the cap the CLI was launched with,
/// larger than this call's budget (review 2 M3 keeps the run), names the
/// cap it really reached, not smelt's smaller budget. Window 65,536: the
/// first call's budget is half of it, 32,768, the run's launch cap; the
/// reply's 40,000 input tokens leave the second call a room of about
/// 21,000, so its run is kept, and its reply writes 32,768 and stops.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_kept_runs_cut_off_names_the_cap_it_ran_with(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let mut first = vec![json!({"read": true})];
    let mut calls = tool_reply(&[("toolu_l19", "todoread", json!({}))]);
    calls[0] = json!({"emit": api_call(40_000, vec![crate::claude_cli::tests::fake::tool_block("toolu_l19", "todoread", &json!({}))], "tool_use", 5)});
    first.extend(calls);
    first.push(json!({"emit": api_call(40_010, vec![text_block("A long reply")], "max_tokens", 32_768)}));
    first.push(json!({"await_interrupt": true}));
    let fake = FakeCli::new("l19", vec![first]);
    let provider = fake.pick_for(&pool, conversation.id).await;
    super::review1::set_cli_window(&pool, provider, 65_536).await;

    // The turn may end on the full conversation afterwards; the notice is
    // saved first either way.
    let _ = run_turn(&pool, conversation.id, hello()).await;

    assert_eq!(fake.started(), 1, "the run was kept for the smaller cap");
    let notices = cut_off_notices(&pool, conversation.id).await;
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].0, 32_768, "the notice names the cap the CLI ran with: {notices:?}");
}
