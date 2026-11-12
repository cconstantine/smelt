//! SME-439: what a window stop is allowed to teach, and what it leaves
//! alone. The incident these tests start from (the ticket's Finding): a
//! repetition-loop reply stopped `max_tokens` at 355 output tokens, and
//! the call's whole measured size — 160,799, how big that conversation
//! happened to be — was saved as strata's Context window for 1.7 hours,
//! while the server had been reporting 262,144 all along. Each test runs
//! a turn against a mock, because the stop arrives on a real reply's
//! stream; the stop is a llama.cpp kind provider's `max_tokens` tell
//! unless a test names another. Candidates are process state keyed by
//! provider id, and every `#[sqlx::test]` database reuses ids: tests
//! that read or write them forget them first.

use super::*;
use super::overflow::{start_refusal_mock, too_long};
use super::window_stop::{set_learned_context_window, set_reported_context_window};

/// `body` reporting all four usage counts, as a real reply's
/// `message_start` and `message_delta` do. `with_usage` leaves cache
/// creation at zero; the incident measured 679 there, and the gate adds
/// all four numbers, so the tests that reproduce it build them whole.
fn with_usage_all(body: &str, input: i64, cache_read: i64, output: i64, cache_creation: i64) -> String {
    body.replacen(
        r#"{"type":"message_start"}"#,
        &format!(
            r#"{{"type":"message_start","message":{{"usage":{{"input_tokens":{input},"cache_creation_input_tokens":{cache_creation},"cache_read_input_tokens":{cache_read},"output_tokens":1}}}}}}"#
        ),
        1,
    )
    .replacen(r#""delta":{"stop_reason""#, &format!(r#""usage":{{"output_tokens":{output}}},"delta":{{"stop_reason""#), 1)
}

/// The incident's usage (input 1, output 355, cache write 679, cache read
/// 159,764 — measured 160,799) stopped at `max_tokens`.
fn incident_body(text: &str) -> String {
    with_usage_all(&text_reply_body(text), 1, 159_764, 355, 679)
        .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#)
}

fn window_learn_spans<'a>(spans: &'a [opentelemetry_sdk::trace::SpanData]) -> Vec<&'a opentelemetry_sdk::trace::SpanData> {
    spans.iter().filter(|span| span.name == "window_learn").collect()
}

fn span_attribute<'a>(span: &'a opentelemetry_sdk::trace::SpanData, key: &str) -> Option<&'a opentelemetry::Value> {
    let values: Vec<_> = span.attributes.iter().filter(|kv| kv.key.as_str() == key).map(|kv| &kv.value).collect();
    assert!(values.len() <= 1, "{key} recorded more than once: {values:?}");
    values.first().copied()
}

pub(super) fn decisions(spans: &[opentelemetry_sdk::trace::SpanData]) -> Vec<String> {
    window_learn_spans(spans)
        .into_iter()
        .map(|span| match span_attribute(span, "smelt.window.decision") {
            Some(opentelemetry::Value::String(s)) => s.as_str().to_string(),
            other => panic!("a decision on {span:?}: {other:?}"),
        })
        .collect()
}

/// T1 — the incident, run red-first against the schema-only tree. The
/// 2026-11-06 16:14 event on strata: reported 262,144, a `max_tokens`
/// stop after 355 output tokens, usage 1 + 355 + 679 + 159,764 — the
/// measured 160,799 is the conversation's size, not the window's.
/// Nothing is learned, and the turn's belief (so the meter, so the next
/// compaction check) does not move at all.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_looping_stop_far_from_the_reported_window_learns_nothing(pool: PgPool) {
    let _guard = lock_turn_tests();
    crate::turn::overflow::forget_window_stop_candidates();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let body = incident_body("Right up to the room.");
    let requests = start_recording_mock_upstream_of_kind(&pool, vec![body], "llama_cpp").await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    set_reported_context_window(&pool, 262_144).await;
    let mut rx = events::subscribe(conversation.id);

    run_turn(&pool, conversation.id, hello()).await.expect("a window-stopped reply still succeeds");

    assert_eq!(requests.lock().expect("the request log").len(), 1, "a window stop retries nothing");
    assert_eq!(saved_context_window(&pool).await, None, "a learner never writes the user's Context window");
    assert_eq!(saved_learned_context_window(&pool).await, None, "160,799 measured the conversation, not the window");
    assert!(
        !published_windows(drain_events(&mut rx).await).contains(&160_799),
        "the turn's belief stayed where the server said it was"
    );
}

/// T2: the rejected stop is traced — one `window_learn` span naming the
/// measurement, the report it disagreed with, and the decision, and
/// (SME-137's rule) none of the reply's text (SME-439's Expected 6).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_rejected_stop_is_traced_with_its_numbers(pool: PgPool) {
    let _guard = lock_turn_tests();
    crate::turn::overflow::forget_window_stop_candidates();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let body = incident_body("plum-drum-zebra, a marker no span may carry.");
    start_recording_mock_upstream_of_kind(&pool, vec![body], "llama_cpp").await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    set_reported_context_window(&pool, 262_144).await;

    let (result, spans) = crate::telemetry::capture_spans(run_turn(&pool, conversation.id, hello())).await;
    result.expect("the turn still succeeds");

    let found = window_learn_spans(&spans);
    let [span] = found.as_slice() else { panic!("one window_learn span: {found:?}") };
    assert_eq!(span_attribute(span, "conversation_id"), Some(&conversation.id.into()));
    assert_eq!(span_attribute(span, "smelt.model"), Some(&"mock-model".into()));
    assert_eq!(span_attribute(span, "smelt.window.source"), Some(&"window_stop".into()));
    assert_eq!(span_attribute(span, "smelt.window.decision"), Some(&"candidate".into()));
    assert_eq!(span_attribute(span, "smelt.window.value"), Some(&160_799_i64.into()));
    assert_eq!(span_attribute(span, "smelt.window.measured_input"), Some(&1_i64.into()));
    assert_eq!(span_attribute(span, "smelt.window.measured_output"), Some(&355_i64.into()));
    assert_eq!(span_attribute(span, "smelt.window.cache_creation"), Some(&679_i64.into()));
    assert_eq!(span_attribute(span, "smelt.window.cache_read"), Some(&159_764_i64.into()));
    assert_eq!(span_attribute(span, "smelt.window.reported"), Some(&262_144_i64.into()));
    assert_eq!(span_attribute(span, "smelt.window.reported_per_request"), Some(&262_144_i64.into()));
    assert!(!format!("{span:?}").contains("plum-drum-zebra"), "no reply text in the span: {span:?}");
}

/// T3: a stop measured *at* the reported window is the room the server
/// has — plausible within the margin is learned, in the learned column,
/// reason and conversation recorded, meter and all.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_max_tokens_stop_measured_at_the_reported_window_learns(pool: PgPool) {
    let _guard = lock_turn_tests();
    crate::turn::overflow::forget_window_stop_candidates();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let body = incident_body("Right up to the room.");
    start_recording_mock_upstream_of_kind(&pool, vec![body], "llama_cpp").await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    set_reported_context_window(&pool, 161_000).await;
    let mut rx = events::subscribe(conversation.id);

    let (result, spans) = crate::telemetry::capture_spans(run_turn(&pool, conversation.id, hello())).await;
    result.expect("the turn succeeds");

    let (learned, reason, conversation_id) = learned_window_record(&pool).await;
    assert_eq!(learned, Some(160_799), "within 4,096 of the reported window is the window");
    assert_eq!(reason.as_deref(), Some("window_stop"), "why the number says it is");
    assert_eq!(conversation_id, Some(conversation.id), "and which conversation taught it");
    assert_eq!(saved_context_window(&pool).await, None, "the user's column stays theirs");
    assert_eq!(decisions(&spans), ["saved"], "and the trace says saved");
    assert!(published_windows(drain_events(&mut rx).await).contains(&160_799), "the meter reads the new window too");
}

/// T4: a window stop never stomps the user's value — a plausible save
/// lands beside their 262,144, not on it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_window_stop_never_stomps_the_users_value(pool: PgPool) {
    let _guard = lock_turn_tests();
    crate::turn::overflow::forget_window_stop_candidates();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let body = with_usage_all(&text_reply_body("Nearly the window."), 260_000, 100, 800, 100)
        .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#);
    start_recording_mock_upstream_of_kind(&pool, vec![body], "llama_cpp").await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    super::window_stop::set_context_window(&pool, 262_144, None).await;
    set_reported_context_window(&pool, 262_144).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the turn succeeds");

    assert_eq!(saved_learned_context_window(&pool).await, Some(261_000), "261,000 is within the margin of 262,144");
    assert_eq!(saved_context_window(&pool).await, Some(262_144), "the user's own value survives its own learner");
}

/// T5: a named refusal — the other learner — recovers its turn under the
/// new storage and still never touches the user's column (the ticket's
/// Done-when clause, pinned under the split).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_named_refusal_never_stomps_the_users_value_and_still_recovers(pool: PgPool) {
    let _guard = lock_turn_tests();
    crate::turn::overflow::forget_window_stop_candidates();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let upstream = start_refusal_mock(&pool, vec![too_long(12_000, 131_072), Ok(text_reply_body("Here you are."))]).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    super::window_stop::set_context_window(&pool, 262_144, None).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the refusal recovers");

    assert_eq!(upstream.count(), 2, "one refusal, one retry");
    let saved = db::list_messages(&pool, conversation.id).await.expect("messages");
    assert_eq!(last_reply(&saved).as_deref(), Some("Here you are."), "the retry answers");
    assert_eq!(saved_learned_context_window(&pool).await, Some(131_072), "the named window is learned");
    assert_eq!(saved_context_window(&pool).await, Some(262_144), "the user's value was never its target");
}

/// T6: a repetition loop measures nothing of the room however plausible
/// its size — the degenerate tell vetoes it before the gate is even
/// consulted.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_degenerate_reply_teaches_nothing_though_its_size_is_plausible(pool: PgPool) {
    let _guard = lock_turn_tests();
    crate::turn::overflow::forget_window_stop_candidates();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let babble = format!("A loop. {}", "finish. ".repeat(60));
    let body = with_usage_all(&text_reply_body(&babble), 1, 159_764, 355, 679)
        .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#);
    start_recording_mock_upstream_of_kind(&pool, vec![body], "llama_cpp").await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    set_reported_context_window(&pool, 161_000).await;

    let (result, spans) = crate::telemetry::capture_spans(run_turn(&pool, conversation.id, hello())).await;
    result.expect("the babbling reply is kept as-is");

    assert_eq!(saved_learned_context_window(&pool).await, None, "a loop is not a measurement");
    assert_eq!(saved_context_window(&pool).await, None, "neither column moves");
    assert_eq!(decisions(&spans), ["ignored_degenerate"]);
}

/// T7: a near-empty reply measured almost nothing of the room: the short
/// floor rejects it whatever its size says.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_very_short_stop_is_ignored(pool: PgPool) {
    let _guard = lock_turn_tests();
    crate::turn::overflow::forget_window_stop_candidates();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let body = with_usage_all(&text_reply_body("Barely a reply."), 1, 159_760, 20, 18)
        .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#);
    start_recording_mock_upstream_of_kind(&pool, vec![body], "llama_cpp").await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    set_reported_context_window(&pool, 161_000).await;

    let (result, spans) = crate::telemetry::capture_spans(run_turn(&pool, conversation.id, hello())).await;
    result.expect("the turn succeeds");

    assert_eq!(saved_learned_context_window(&pool).await, None, "20 output tokens measured nothing");
    assert_eq!(decisions(&spans), ["ignored_short"]);
}

/// T8: with nothing reported, one stop is evidence for its turn only —
/// the meter and this turn's compaction read it, nobody else ever does,
/// and the candidate waits in case it happens again.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_one_stop_with_nothing_reported_is_evidence_for_its_turn_only(pool: PgPool) {
    let _guard = lock_turn_tests();
    crate::turn::overflow::forget_window_stop_candidates();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let body = with_usage_all(&text_reply_body("Filled what there was."), 41_900, 0, 100, 0)
        .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#);
    start_recording_mock_upstream_of_kind(&pool, vec![body], "llama_cpp").await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let mut rx = events::subscribe(conversation.id);

    let (result, spans) = crate::telemetry::capture_spans(run_turn(&pool, conversation.id, hello())).await;
    result.expect("the turn succeeds");

    assert_eq!(saved_learned_context_window(&pool).await, None, "one stop, no report: nothing is saved");
    assert_eq!(saved_context_window(&pool).await, None, "neither column moves");
    assert_eq!(decisions(&spans), ["turn_only"]);
    assert!(published_windows(drain_events(&mut rx).await).contains(&42_000), "the turn's copy did move: the meter reads 42,000");
}

/// T9: two like-sized stops are the evidence one was not — the repeat
/// rule saves what the first stop only believed for itself.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_second_like_sized_stop_saves_what_the_first_only_hoped(pool: PgPool) {
    let _guard = lock_turn_tests();
    crate::turn::overflow::forget_window_stop_candidates();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let body = with_usage_all(&text_reply_body("Filled what there was again."), 41_900, 0, 100, 0)
        .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#);
    start_recording_mock_upstream_of_kind(&pool, vec![body], "llama_cpp").await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    let (result, spans) = crate::telemetry::capture_spans(async {
        run_turn(&pool, conversation.id, hello()).await.expect("the first stop's turn succeeds");
        run_turn(&pool, conversation.id, hello()).await
    })
    .await;
    result.expect("the second stop's turn succeeds too");

    assert_eq!(decisions(&spans), ["turn_only", "saved"], "the repeat is the evidence");
    assert_eq!(saved_learned_context_window(&pool).await, Some(42_000), "twice 42,000 is 42,000");
}

/// T10: the repeat has to be like-sized: a second stop at a different
/// size replaces the candidate and still saves nothing.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_second_stop_of_a_different_size_replaces_the_candidate(pool: PgPool) {
    let _guard = lock_turn_tests();
    crate::turn::overflow::forget_window_stop_candidates();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let small = with_usage_all(&text_reply_body("Filled some."), 41_900, 0, 100, 0)
        .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#);
    let big = with_usage_all(&text_reply_body("Filled more."), 119_900, 0, 100, 0)
        .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#);
    start_recording_mock_upstream_of_kind(&pool, vec![small, big], "llama_cpp").await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    let (result, spans) = crate::telemetry::capture_spans(async {
        run_turn(&pool, conversation.id, hello()).await.expect("the first stop's turn succeeds");
        run_turn(&pool, conversation.id, hello()).await
    })
    .await;
    result.expect("the second stop's turn succeeds too");

    assert_eq!(decisions(&spans), ["turn_only", "turn_only"], "120,000 is no repeat of 42,000");
    assert_eq!(saved_learned_context_window(&pool).await, None, "nothing is saved from two different sizes");
}

/// Round 1 M1: a repeat that saves must come from a later turn, not from
/// the same turn calling the model again. `carries_on` (and a looping
/// reply that keeps restopping — this incident's own shape) can stop a
/// user turn's second model call at the same size, and that second stop
/// measured the same conversation filling, not a second, independent
/// fill. The cross-turn case — a second turn's like-sized stop being the
/// evidence — stays T9, pinned at the learner's own level below.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_like_sized_stop_inside_one_turn_is_no_repeat(pool: PgPool) {
    let _guard = lock_turn_tests();
    crate::turn::overflow::forget_window_stop_candidates();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    // The llama.cpp shape, nothing reported: only a repeat could ever
    // save. The learner is driven once per model call of one turn; the
    // mock is there to own the provider row, not to answer.
    let _requests = start_recording_mock_upstream_of_kind(&pool, vec![text_reply_body("Never served.")], "llama_cpp").await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let mut turn_model = crate::providers::resolve_turn_model(&pool, conversation.id).await.expect("the mock model");
    let usage = anthropic::TokenUsage { input_tokens: 41_900, output_tokens: 100, ..Default::default() };
    let content = [anthropic::ContentBlock::Text { text: "Filled what there was.".into() }];

    // Two model calls inside one turn (carries_on, say), both stopped at
    // 42,000: one turn id for both.
    crate::turn::overflow::learn_window_stop(&pool, conversation.id, 1, &mut turn_model, &usage, &content, "max_tokens").await;
    crate::turn::overflow::learn_window_stop(&pool, conversation.id, 1, &mut turn_model, &usage, &content, "max_tokens").await;
    assert_eq!(saved_learned_context_window(&pool).await, None, "one turn, two like-sized stops: one conversation filled, not two fills");

    // The same stop in a later turn is the evidence (T9's pair, pinned at
    // the learner's own level).
    crate::turn::overflow::learn_window_stop(&pool, conversation.id, 2, &mut turn_model, &usage, &content, "max_tokens").await;
    assert_eq!(saved_learned_context_window(&pool).await, Some(42_000), "twice 42,000, two turns apart, is 42,000");
}

/// T11: a llama.cpp server's `n_ctx` is the total across its slots — the
/// gate divides before it compares. A measurement at one slot's window
/// (262,144 ÷ 2 = 131,072) is plausible at 130,000, and the incident's
/// 160,799 — the whole conversation, nowhere near one slot — is not.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_two_slot_server_reports_its_window_per_slot(pool: PgPool) {
    let _guard = lock_turn_tests();
    crate::turn::overflow::forget_window_stop_candidates();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let body = with_usage_all(&text_reply_body("A slot filled."), 129_900, 0, 100, 0)
        .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#);
    start_recording_mock_upstream_of_kind(&pool, vec![body], "llama_cpp").await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    set_reported_context_window(&pool, 262_144).await;
    // `server_caps` parses into `LlamaServerInfo`, which wants its
    // template-cap map (even empty) — seed a value it will read.
    sqlx::query("UPDATE inference_providers SET server_caps = $1")
        .bind(serde_json::json!({ "total_slots": 2, "template_caps": {} }))
        .execute(&pool)
        .await
        .expect("seed the slot count");

    run_turn(&pool, conversation.id, hello()).await.expect("the turn succeeds");

    assert_eq!(saved_learned_context_window(&pool).await, Some(130_000), "131,072 per slot is 130,000's neighbour");
}

/// T11's other half: the incident's number is not a slot's window.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_two_slot_server_finds_an_incident_sized_stop_far(pool: PgPool) {
    let _guard = lock_turn_tests();
    crate::turn::overflow::forget_window_stop_candidates();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let body = incident_body("Right up to the room.");
    start_recording_mock_upstream_of_kind(&pool, vec![body], "llama_cpp").await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    set_reported_context_window(&pool, 262_144).await;
    sqlx::query("UPDATE inference_providers SET server_caps = $1")
        .bind(serde_json::json!({ "total_slots": 2, "template_caps": {} }))
        .execute(&pool)
        .await
        .expect("seed the slot count");

    run_turn(&pool, conversation.id, hello()).await.expect("the turn succeeds");

    assert_eq!(saved_learned_context_window(&pool).await, None, "160,799 is no slot's 131,072");
}

/// T12: `model_context_window_exceeded` is the server's own claim the
/// window filled — unambiguous, learned ungated (still vetoed by the
/// short/degenerate floors, pinned by T6/T7 shapes).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_named_window_stop_still_learns_ungated(pool: PgPool) {
    let _guard = lock_turn_tests();
    crate::turn::overflow::forget_window_stop_candidates();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let body = with_usage(&text_reply_body("Half a thought."), 30_000, 0, 5_000)
        .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"model_context_window_exceeded""#);
    start_recording_mock_upstream(&pool, vec![body]).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    set_reported_context_window(&pool, 64_000).await;
    let mut rx = events::subscribe(conversation.id);

    run_turn(&pool, conversation.id, hello()).await.expect("the turn succeeds");

    assert_eq!(saved_learned_context_window(&pool).await, Some(35_000), "far from the report, and still: the server said the window filled");
    assert!(published_windows(drain_events(&mut rx).await).contains(&35_000), "the meter reads it too");
}

/// PR 2, Expected 3b: a whole reply that filled more than the learned
/// window just fit through it — the learned number is too small and goes.
/// Reported 262,144 keeps the turn's belief out of the way; the reply ends
/// clean, so no stop learner is involved at all. Round 1 H1: the clear
/// speaks on `window_learn` — decision `window_beaten`, the whole call's
/// numbers, no conversation id to tell (the turn loop isn't this clear's
/// to widen), and no reply text (SME-137's rule, kept).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_past_the_learned_window_forgives_it(pool: PgPool) {
    let _guard = lock_turn_tests();
    crate::turn::overflow::forget_window_stop_candidates();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let body = with_usage(&text_reply_body("plum-drum-zebra, over the learned line."), 39_900, 0, 100);
    start_recording_mock_upstream_of_kind(&pool, vec![body], "llama_cpp").await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    set_reported_context_window(&pool, 262_144).await;
    set_learned_context_window(&pool, 32_768).await;

    let (result, spans) = crate::telemetry::capture_spans(run_turn(&pool, conversation.id, hello())).await;
    result.expect("the turn succeeds");

    assert_eq!(saved_learned_context_window(&pool).await, None, "40,000 measured fit: 32,768 was too small a claim");
    assert_eq!(saved_context_window(&pool).await, None, "a clear's no one to move the user's column");

    let found = window_learn_spans(&spans);
    let [span] = found.as_slice() else { panic!("one window_learn span for the clear: {found:?}") };
    assert_eq!(span_attribute(span, "smelt.window.source"), Some(&"reply".into()));
    assert_eq!(span_attribute(span, "smelt.window.decision"), Some(&"window_beaten".into()));
    assert_eq!(span_attribute(span, "smelt.window.value"), Some(&40_000_i64.into()));
    assert_eq!(span_attribute(span, "smelt.window.measured_input"), Some(&39_900_i64.into()));
    assert_eq!(span_attribute(span, "smelt.window.measured_output"), Some(&100_i64.into()));
    assert_eq!(span_attribute(span, "conversation_id"), None, "the clear has no conversation in hand and reaches for none");
    assert!(!format!("{span:?}").contains("plum-drum-zebra"), "no reply text in the span: {span:?}");
}

/// Expected 3b's other half: a reply that stayed inside the learned window
/// says nothing against it — 30,000 inside 32,768 keeps the telling. And
/// (round 1 H1) says nothing on the trace either: the clear fires on the
/// UPDATE's answer, never on the attempt.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_within_the_learned_window_keeps_it(pool: PgPool) {
    let _guard = lock_turn_tests();
    crate::turn::overflow::forget_window_stop_candidates();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let body = with_usage(&text_reply_body("Well inside."), 29_900, 0, 100);
    start_recording_mock_upstream_of_kind(&pool, vec![body], "llama_cpp").await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    set_reported_context_window(&pool, 262_144).await;
    set_learned_context_window(&pool, 32_768).await;

    let (result, spans) = crate::telemetry::capture_spans(run_turn(&pool, conversation.id, hello())).await;
    result.expect("the turn succeeds");

    assert_eq!(saved_learned_context_window(&pool).await, Some(32_768), "fitting inside the claim is no proof against it");
    assert!(window_learn_spans(&spans).is_empty(), "a clear that clears nothing traces nothing: {:?}", window_learn_spans(&spans));
}

/// The per-request division: llama.cpp splits its total across slots,
/// everyone else reports per model, and nothing reported is not zero.
#[test]
fn test_the_reported_window_divides_by_slots_for_llama_cpp_only() {
    use crate::providers::ProviderKind;
    let per_request = |reported, kind, slots| crate::turn::overflow::reported_per_request(reported, kind, slots);
    assert_eq!(per_request(None, ProviderKind::LlamaCpp, Some(2)), None, "nothing reported proves nothing");
    assert_eq!(per_request(Some(0), ProviderKind::LlamaCpp, Some(2)), None, "a zero report isn't a window");
    assert_eq!(per_request(Some(262_144), ProviderKind::LlamaCpp, None), Some(262_144), "no slot info: pass it whole");
    assert_eq!(per_request(Some(262_144), ProviderKind::LlamaCpp, Some(0)), Some(262_144), "a zero slot count divides nothing");
    assert_eq!(per_request(Some(262_144), ProviderKind::LlamaCpp, Some(1)), Some(262_144), "one slot keeps the total");
    assert_eq!(per_request(Some(262_144), ProviderKind::LlamaCpp, Some(2)), Some(131_072), "two slots split it");
    assert_eq!(per_request(Some(262_144), ProviderKind::Anthropic, Some(2)), Some(262_144), "other kinds report per model");
}

/// The proximity margin: within it is plausible, outside and absent both
/// are not.
#[test]
fn test_plausibility_reads_only_within_the_margin() {
    let plausible = crate::turn::overflow::stop_reads_plausible;
    assert!(plausible(160_799, Some(161_000)), "201 off reads as the window");
    assert!(plausible(160_799, Some(156_703)), "the margin boundary itself (4,096 off) is plausible");
    assert!(!plausible(160_799, Some(156_702)), "one past it isn't");
    assert!(!plausible(160_799, Some(262_144)), "the incident's disagreement isn't");
    assert!(!plausible(160_799, None), "nothing reported proves nothing");
}

/// The repetition-loop tell: the incident's babble and normal prose on
/// either side, and small repeats stay normal.
#[test]
fn test_the_loop_tell_reads_babble_not_prose() {
    let says = crate::turn::overflow::looks_degenerate;
    assert!(says(&"finish. ".repeat(60)), "the incident's ending");
    assert!(says(&format!("A measured start. {}", "finish. ".repeat(30))), "a loop after a normal start");
    assert!(!says("Right up to the room. The end."), "normal prose isn't");
    assert!(!says("the the"), "a small repeat isn't a loop");
    assert!(!says(&"finish. ".repeat(23)), "23 repeats stays short of the tell");
}
