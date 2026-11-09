//! A misnamed tool call on a Claude Code provider (SME-447): the CLI
//! refuses a name it was never given locally, and smelt reads that echo as
//! the same slip an HTTP kind gets its unknown-call correction for — the
//! turn carries on, and the shared 4-in-a-row bound applies. A CLI-answered
//! call of an offered name is still a divergence.

use super::*;
use crate::claude_cli::tests::fake::tool_block;

/// An `init` line saying the CLI can call `mcp__smelt__todoread`: `note`
/// keeps that list (the only check a refusal echo is judged against), and
/// it satisfies the bridge-connection check.
fn cli_init() -> Value {
    json!({"type": "system", "subtype": "init", "session_id": "00000000-0000-4000-8000-000000000001",
           "tools": ["mcp__smelt__todoread"],
           "mcp_servers": [{"name": "smelt", "status": "connected"}]})
}

/// A `user` event echoing the CLI's own local refusal of `id`, the shape
/// it writes for a name it doesn't list — never seen by the bridge.
fn refusal(id: &str) -> Value {
    json!({"type": "user", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": id, "is_error": true, "content": "refused locally"}
    ]}})
}

/// An API call whose reply calls `mcp__smelt__get_issue`, a name the CLI
/// was never given.
fn bad_call(id: &str) -> Value {
    json!({"emit": api_call(10, vec![tool_block(id, "get_issue", &json!({}))], "tool_use", 5)})
}

/// Everything saved, serialized, for the "the CLI's words never reach
/// smelt's record" assertion.
fn saved_json(saved: &[(String, Vec<anthropic::ContentBlock>)]) -> String {
    serde_json::to_string(saved).expect("saved messages serialize")
}

/// The content of the tool result for `id` among `saved`.
fn tool_result_for(saved: &[(String, Vec<anthropic::ContentBlock>)], id: &str) -> Option<String> {
    saved.iter().flat_map(|(_, blocks)| blocks.iter()).find_map(|block| match block {
        anthropic::ContentBlock::ToolResult { tool_use_id, content, .. } if tool_use_id == id => Some(content.clone()),
        _ => None,
    })
}

/// The slip: the CLI's local refusal of an unknown name carries the turn
/// on to the model's next reply in the same run; smelt saves its own
/// correction as the call's result, never the CLI's words.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_unknown_name_the_cli_refused_carries_the_turn_on(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("unknown-carries", vec![vec![
        json!({"read": true}),
        json!({"emit": [cli_init()]}),
        bad_call("toolu_bad"),
        json!({"emit": [refusal("toolu_bad")]}),
        text_reply("Recovered."),
    ]]);
    fake.pick_for(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the turn carries on");

    assert_eq!(fake.started(), 1, "the same run carried on, no restart");
    let saved = saved(&pool, conversation.id).await;
    // The reply rides in the record under the model's own name — the CLI
    // was never given it, so nothing maps it back.
    assert!(saved.iter().any(|(role, blocks)| role == "assistant"
        && matches!(&blocks[..], [anthropic::ContentBlock::ToolUse { id, name, .. }]
            if id == "toolu_bad" && name == "mcp__smelt__get_issue")), "{saved:?}");
    // The call's result is smelt's SME-307 correction, not the CLI's text.
    let result = tool_result_for(&saved, "toolu_bad").expect("the slip's result");
    assert!(result.contains("mcp__smelt__get_issue") && result.contains("isn't an offered tool"), "{result}");
    // And the model went on in the same turn.
    assert!(saved.iter().any(|(role, blocks)| role == "assistant"
        && matches!(&blocks[..], [anthropic::ContentBlock::Text { text }] if text == "Recovered.")), "{saved:?}");
    assert!(!saved_json(&saved).contains("refused locally"), "the CLI's refusal text never reaches the record: {saved:?}");
}

/// The shared bound: four unknown names in a row end the turn with
/// `unoffered_bound_error`, after three slips carried on within the one
/// process.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_four_unknown_names_in_a_row_end_the_turn(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let mut steps = vec![json!({"read": true}), json!({"emit": [cli_init()]})];
    for n in 1..=4 {
        let id = format!("toolu_bad{n}");
        steps.push(bad_call(&id));
        steps.push(json!({"emit": [refusal(&id)]}));
    }
    let fake = FakeCli::new("unknown-bound", vec![steps]);
    fake.pick_for(&pool, conversation.id).await;

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("the bound ends the turn");
    assert!(error.to_string().contains("times in a row"), "{error}");
    assert!(error.to_string().contains("`mcp__smelt__get_issue`"), "the bound names the call: {error}");
    assert_eq!(fake.started(), 1, "the three slips before the bound were the same run");
}

/// A real call resets the unknown count: three slips, a real call, three
/// more slips — six slips, never four in a row, so the turn completes.
/// If the reset were broken the first slip after the real call, the 4th in
/// a row, would end the turn at the bound.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_real_call_resets_the_unknown_count(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let mut steps = vec![json!({"read": true}), json!({"emit": [cli_init()]})];
    for n in 1..=3 {
        let id = format!("toolu_bad{n}");
        steps.push(bad_call(&id));
        steps.push(json!({"emit": [refusal(&id)]}));
    }
    steps.extend(tool_reply(&[("toolu_ok", "todoread", json!({}))]));
    for n in 4..=6 {
        let id = format!("toolu_bad{n}");
        steps.push(bad_call(&id));
        steps.push(json!({"emit": [refusal(&id)]}));
    }
    steps.push(text_reply("Done."));
    let fake = FakeCli::new("unknown-reset", vec![steps]);
    fake.pick_for(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("six slips, never four in a row");

    assert_eq!(fake.started(), 1, "one run for the whole turn");
    let saved = saved(&pool, conversation.id).await;
    assert!(saved.iter().any(|(role, blocks)| role == "assistant"
        && matches!(&blocks[..], [anthropic::ContentBlock::Text { text }] if text == "Done.")), "{saved:?}");
    assert!(!saved_json(&saved).contains("refused locally"), "{saved:?}");
}

/// Decision 1(b) with an `init` seen: the rule is the name, not the
/// echo's wording — a CLI that answers a call of an offered name itself
/// is still a divergence, and fails the turn.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_cli_answer_for_an_offered_name_still_diverges(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("unknown-diverges", vec![vec![
        json!({"read": true}),
        json!({"emit": [cli_init()]}),
        json!({"emit": api_call(10, vec![tool_block("toolu_here", "todoread", &json!({}))], "tool_use", 5)}),
        json!({"emit": [refusal("toolu_here")]}),
        text_reply("Unrelated."),
    ]]);
    fake.pick_for(&pool, conversation.id).await;

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("diverged");
    assert!(error.to_string().contains("answered tool call toolu_here itself"), "{error}");
    let saved = saved(&pool, conversation.id).await;
    assert!(!saved.iter().any(|(_, blocks)| blocks.iter().any(|b| matches!(b, anthropic::ContentBlock::Text { text } if text == "Unrelated."))), "{saved:?}");
}
