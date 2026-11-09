//! SME-443: the compaction summary's budget — room derived from the model's
//! window and the request's size, one continuation for a summary cut off at
//! it, and refusal of what's still unusable (empty on every kind; still cut
//! off after the one continuation). Fake upstreams throughout:
//! `start_recording_mock_upstream` answers each request with the next body
//! and logs every request parsed, so the summary call's `max_tokens`, the
//! continuation's message shape and the exact join are pinned on the wire.

use super::overflow::compaction_summaries;
use super::text_calls::{text_blocks_body, Block};
use super::*;

/// A conversation already past the compaction ceiling on the mock's default
/// 200k window (`compaction_honesty`'s pattern): a seeded pair and usage
/// past the trigger, so the turn compacts before answering. Returns its id.
async fn conversation_past_the_trigger(pool: &PgPool) -> i64 {
    let conversation = db::create_conversation(pool).await.expect("create conversation");
    db::create_message(
        pool,
        conversation.id,
        "user",
        &[anthropic::ContentBlock::Text { text: "earlier".to_string() }],
    )
    .await
    .expect("seed earlier");
    db::create_message(
        pool,
        conversation.id,
        "assistant",
        &[anthropic::ContentBlock::Text { text: "reply".to_string() }],
    )
    .await
    .expect("seed reply");
    db::upsert_conversation_usage(
        pool,
        conversation.id,
        &anthropic::TokenUsage {
            input_tokens: 190_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    )
    .await
    .expect("seed usage");
    conversation.id
}

/// A turn model for the pure formula tests: `context_window`, the applied
/// `output_cap` (the user's "Max reply tokens" can lower the model's own,
/// SME-111), and `reported`, the cap the model itself reported — which
/// clamps the summary whatever the user's setting did to it (SME-443
/// review 1).
fn room_model(context_window: u32, output_cap: Option<u32>, reported: Option<u32>) -> crate::providers::TurnModel {
    crate::providers::TurnModel {
        endpoint: crate::anthropic::stream::Endpoint {
            base_url: "http://room.test".to_string(),
            auth: crate::anthropic::stream::Auth::ApiKey("test".to_string()),
            slots_of: None,
        },
        provider_id: 1,
        model: "room-test-model".to_string(),
        thinking: false,
        prompt_caching: false,
        price_catalog_provider: None,
        effort: None,
        kind: crate::providers::ProviderKind::Anthropic,
        output_cap,
        reasoning_cap: None,
        output_cap_is_models_own: reported.is_some() && output_cap == reported,
        reported_max_output: reported,
        template: None,
        context_window,
        thinking_stripped_through: None,
        adaptive_thinking: true,
    }
}

/// A8: the derived summary budget, exact numbers (`compaction_progress`'s
/// "value itself" style). room = window − request_chars/3 − 4,096, floored
/// at 2,048, clamped at the 8,192 ceiling and the model's reported output
/// cap. *Expected red before the fix (the stub returned today's constant
/// for every case): `left: 2048, right: 8192`. Before review 1's clamp
/// fix, the user-below-reported case: `left: 8192, right: 4096`.*
#[test]
fn test_the_summary_budget_formula() {
    use crate::turn::compaction::summary_max_tokens;
    // A roomy window and a small request: room dwarfs the ceiling → 8,192.
    assert_eq!(summary_max_tokens(&room_model(200_000, None, None), 3_000), 8_192);
    // Room between the floor and the ceiling is sent as it is:
    // 20,000 − 30,000/3 − 4,096 = 5,904.
    assert_eq!(summary_max_tokens(&room_model(20_000, None, None), 30_000), 5_904);
    // A stingy room loses to the 2,048 floor:
    // 12,000 − 20,000/3 − 4,096 = 1,238 → 2,048. The request, not the
    // budget, is what `compact`'s overflow arm then answers.
    assert_eq!(summary_max_tokens(&room_model(12_000, None, None), 20_000), 2_048);
    // A request past the whole window: room clamps at 1, the floor still
    // holds — the overflow arm owns that request, not the budget.
    assert_eq!(summary_max_tokens(&room_model(16_384, None, None), 60_000), 2_048);
    // A model's own reported cap clamps the result: Anthropic 400s above it.
    assert_eq!(summary_max_tokens(&room_model(200_000, Some(4_096), Some(4_096)), 3_000), 4_096);
    // Below the floor, the model's own cap still wins — sending a request
    // the provider would 400 on is worse than a short summary budget.
    assert_eq!(summary_max_tokens(&room_model(200_000, Some(1_024), Some(1_024)), 3_000), 1_024);
    // The user's "Max reply tokens" below the model's own cap neither
    // applies to the summary (it is not a reply) nor skips its clamp:
    // that setting flips `output_cap_is_models_own` false, and sending
    // the ceiling over the model's own 4,096 is the 400 no arm answers
    // (review 1, Medium).
    assert_eq!(summary_max_tokens(&room_model(200_000, Some(2_000), Some(4_096)), 3_000), 4_096);
    // A user cap with no reported cap at all still does not apply: the
    // ceiling holds.
    assert_eq!(summary_max_tokens(&room_model(200_000, Some(2_000), None), 3_000), 8_192);
}

/// SME-443 review 1 (Medium), on the wire: the model reports 4,096, the
/// user's "Max output" is 2,000 (so `output_cap_is_models_own` is false
/// and the applied cap is the user's), and a long conversation compacts —
/// the summary request must stay at the model's own 4,096. Above it
/// Anthropic answers "maximum allowed", a 400 `compact`'s overflow arm
/// deliberately does not answer, so every send of the conversation would
/// fail raw. *Expected red before the fix: `left: Some(8192),
/// right: Some(4096)`.*
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_summary_budget_stays_under_the_models_own_cap_when_the_users_is_lower(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation_id = conversation_past_the_trigger(&pool).await;
    let requests = start_recording_mock_upstream(&pool, vec![
        text_reply_body("Summary: nothing live."),
        text_reply_body("Carrying on."),
    ])
    .await;
    // After the mock provider exists — the SELECT joins `inference_providers`,
    // and zero rows would cap nothing (cut_off.rs's order).
    let inserted = sqlx::query("INSERT INTO provider_models (provider_id, model, reported_max_output, max_output) SELECT id, $1, 4096, 2000 FROM inference_providers")
        .bind(crate::providers::test_support::MOCK_MODEL)
        .execute(&pool)
        .await
        .expect("reported cap 4,096, user \"Max output\" 2,000");
    assert!(inserted.rows_affected() >= 1, "the mock provider's model row was capped");

    run_turn(&pool, conversation_id, hello())
        .await
        .expect("the compaction fits the model's own cap and the turn carries on");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 2, "a compaction call, then the real turn");
    assert_eq!(
        requests[0]["max_tokens"].as_u64(),
        Some(4_096),
        "the model's reported cap clamps the summary budget even when a lower user \"Max output\" masks it (SME-443 review 1)"
    );
}

/// A1: on the wire. A compaction against a default-200k-window model with a
/// small transcript sends the derived ceiling, not the old 2,048 constant —
/// the summary gets room to finish (SME-443). *Expected red before the fix:
/// `left: Some(2048), right: Some(8192)`.*
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_summary_budget_grows_with_the_room(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation_id = conversation_past_the_trigger(&pool).await;
    let requests = start_recording_mock_upstream(&pool, vec![
        text_reply_body("Summary: nothing live."),
        text_reply_body("Carrying on."),
    ])
    .await;

    run_turn(&pool, conversation_id, hello())
        .await
        .expect("the compaction runs and the turn carries on");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 2, "a compaction call, then the real turn");
    assert_eq!(
        requests[0]["max_tokens"].as_u64(),
        Some(8_192),
        "the summary asks for the room-derived ceiling, not a fixed 2,048 (SME-443)"
    );
}

/// A2: an HTTP summary cut off at its budget is continued **once**, and
/// the join saves whole — verbatim, no separator, no marker (SME-443;
/// Q3 decided the verbatim join). *Expected red before the fix: two
/// requests, saved text = part1 + marker.*
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_cut_off_summary_is_continued_once_and_saved_whole(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation_id = conversation_past_the_trigger(&pool).await;
    // Mid-sentence at the cap, the strata shape: half a Planned line.
    let part1 = "Done: split the file, proof green. Planned: conti";
    let rest = "nue once, then refuse the unusable.";
    let cut = text_reply_body(part1).replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#);
    assert!(cut.contains("max_tokens"), "shape guard: the summary is cut off: {cut}");
    let requests = start_recording_mock_upstream(&pool, vec![
        cut,
        text_reply_body(rest),
        text_reply_body("Carrying on."),
    ])
    .await;

    run_turn(&pool, conversation_id, hello())
        .await
        .expect("the continuation saves the summary whole and the turn carries on");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 3, "summary, continuation, then the real turn");
    let msgs = requests[1]["messages"].as_array().expect("the continuation's messages");
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().expect("a role")).collect();
    assert_eq!(roles, ["user", "assistant", "user"], "the request ends on user — no prefill (SME-443)");
    assert_eq!(msgs[1]["content"][0]["text"].as_str(), Some(part1), "part 1 rides as the assistant message");
    assert!(
        msgs[2]["content"][0]["text"].as_str().unwrap_or_default().contains("never repeat it"),
        "the condition prompt is the third message: {:?}",
        msgs[2]
    );
    assert_eq!(requests[1]["max_tokens"].as_u64(), Some(8_192), "the continuation gets a fresh derived budget");
    drop(requests);

    let saved = compaction_summaries(&pool, conversation_id).await;
    assert_eq!(saved.len(), 1, "one divider");
    assert_eq!(saved[0], format!("{part1}{rest}"), "the verbatim join, no separator (Q3 decided)");
}

/// A4: the strata shape — thinking spent the budget, the text stopped
/// mid-record — gets room on the first call and the one continuation;
/// thinking blocks are never saved, only the first Text (SME-443).
/// *Expected red before the fix: saved = part1 + marker, 2 requests.*
/// (The plan's `with_usage(…, output 2048)` on this body would only
/// change what the recorder stores, not any decision here, so the
/// body keeps its own usage — `text_blocks_body` builds it.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_summary_that_spends_its_budget_on_reasoning_gets_room_and_continuation(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation_id = conversation_past_the_trigger(&pool).await;
    let part1 = "The session so far: goal fixed, tests";
    let rest = " green at the tip.";
    let cut = text_blocks_body(
        &[Block::Thinking("lots of reasoning about the fix"), Block::Text(part1)],
        "max_tokens",
    );
    let requests = start_recording_mock_upstream(&pool, vec![
        cut,
        text_reply_body(rest),
        text_reply_body("Carrying on."),
    ])
    .await;

    run_turn(&pool, conversation_id, hello())
        .await
        .expect("the reasoning-only head still gets its continuation");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 3, "summary, continuation, turn");
    assert_eq!(requests[0]["max_tokens"].as_u64(), Some(8_192), "the first call asked for the room");
    drop(requests);

    let saved = compaction_summaries(&pool, conversation_id).await;
    assert_eq!(saved.len(), 1, "one divider");
    assert_eq!(saved[0], format!("{part1}{rest}"), "first Text only, joined verbatim: {saved:?}");
    assert!(!saved[0].contains("reasoning"), "thinking blocks are never saved: {saved:?}");
}

/// A3: a summary whose continuation is *also* cut off is refused —
/// loudly, with the history kept — rather than saved as a marked
/// torso (SME-443). Exactly two model calls: the once is a once, and a
/// refused compaction marks no progress (the plan's 2c; escape and
/// compaction_progress stay green). *Expected red before the refusal
/// step (with the continuation in, commit 4's stub-back): compaction
/// saves marked, no error.*
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_summary_still_cut_off_after_its_continuation_fails_and_keeps_the_history(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation_id = conversation_past_the_trigger(&pool).await;
    let cut = |t: &str| text_reply_body(t).replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#);
    let requests = start_recording_mock_upstream(&pool, vec![
        cut("Done: split the file. Planned: con"),
        cut("tinue once, then refuse — the torso of a"),
        text_reply_body("Never sent."),
    ])
    .await;

    let error = run_turn(&pool, conversation_id, hello())
        .await
        .expect_err("a summary still cut off after its one continuation fails the compaction")
        .to_string();

    assert!(
        error.contains("cut off again after one continuation"),
        "the refusal names what happened: {error}"
    );
    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 2, "summary, continuation — no third call (the once): {requests:?}");
    drop(requests);
    assert!(
        compaction_summaries(&pool, conversation_id).await.is_empty(),
        "no divider: the history was kept as it is"
    );
}

/// A6: an HTTP summary that comes back **empty** fails the compaction
/// on HTTP kinds too (SME-443 step 3), whatever stopped it — and
/// short-circuits: there is nothing to continue, so no continuation
/// call. *Expected red before the refusal step: empty summary saves
/// unmarked, turn proceeds.*
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_empty_summary_fails_the_compaction_on_http_kinds(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation_id = conversation_past_the_trigger(&pool).await;
    let requests = start_recording_mock_upstream(&pool, vec![
        text_blocks_body(&[], "end_turn"),
        text_reply_body("Never sent."),
    ])
    .await;

    let error = run_turn(&pool, conversation_id, hello())
        .await
        .expect_err("an empty summary is refused, not saved")
        .to_string();

    assert!(error.contains("came back empty"), "the refusal names the empty summary: {error}");
    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 1, "no continuation of an empty, no turn: {requests:?}");
    drop(requests);
    assert!(
        compaction_summaries(&pool, conversation_id).await.is_empty(),
        "no divider: the history was kept as it is"
    );
}

/// A7: a summary that spends its whole budget on reasoning — cut off,
/// with no text at all — is the same refusal as A6: empty beats
/// continuation, there is nothing to continue (SME-443). *Expected red
/// before the refusal step: empty saves with marker.*
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_summary_all_of_reasoning_and_no_text_fails_as_empty(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation_id = conversation_past_the_trigger(&pool).await;
    let requests = start_recording_mock_upstream(&pool, vec![
        text_blocks_body(&[Block::Thinking("every token went here")], "max_tokens"),
        text_reply_body("Never sent."),
    ])
    .await;

    let error = run_turn(&pool, conversation_id, hello())
        .await
        .expect_err("a reasoning-only summary is empty, and empty is refused")
        .to_string();

    assert!(error.contains("came back empty"), "empty beats continuation: {error}");
    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 1, "no continuation without text: {requests:?}");
    drop(requests);
    assert!(
        compaction_summaries(&pool, conversation_id).await.is_empty(),
        "no divider: the history was kept as it is"
    );
}
