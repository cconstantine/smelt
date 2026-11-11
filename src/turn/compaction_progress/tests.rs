//! `CompactionProgress` tests, moved out of its file (SME-414 commit 1: a
//! move only, no test changed; the file sat over the 30,000-byte aim and this
//! ticket adds more tests). The shared helpers stay with the tests.

use super::*;

/// The sentence every stuck wording must end with (SME-423): a raise
/// lives on the server *and* on the provider's page, and the notice has
/// to send the user to both.
const PROVIDERS_PAGE_TAIL: &str = "Give the model a larger context window on its server and on its provider's page, or pick another model.";

const WINDOW: u32 = 100_000;
const TRIGGER: u64 = 83_616;
const LIMIT: u64 = 93_856;

fn input(tokens: u64) -> anthropic::TokenUsage {
    anthropic::TokenUsage {
        input_tokens: tokens as i64,
        output_tokens: 50_000,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
    }
}

fn after_compaction(progress: &mut CompactionProgress, tokens: u64) {
    progress.compacted(WINDOW);
    progress.measured(&input(tokens), WINDOW);
}

fn step(progress: &mut CompactionProgress, projected: u64) -> CompactionStep {
    progress.step("m", WINDOW, Some(projected))
}

#[test]
fn test_the_usual_trigger_applies_until_something_is_learned() {
    let mut progress = CompactionProgress::default();
    assert_eq!(step(&mut progress, TRIGGER - 1), CompactionStep::Go);
    assert_eq!(step(&mut progress, TRIGGER), CompactionStep::Compact);
    assert_eq!(progress.step("m", WINDOW, None), CompactionStep::Go, "nothing measured");
}

/// The value itself, in numbers: the integration tests would pass for
/// anything from 617 to 5,616.
#[test]
fn test_a_compaction_has_to_leave_4096_tokens_under_the_trigger() {
    let mut progress = CompactionProgress::default();
    after_compaction(&mut progress, 79_520);
    assert_eq!(progress.tight_results, 0, "83,616 less 4,096 is enough");
    after_compaction(&mut progress, 79_521);
    assert_eq!(progress.tight_results, 1, "one token more isn't");
}

#[test]
fn test_two_tight_results_in_a_row_move_the_trigger_to_the_limit() {
    let mut progress = CompactionProgress::default();
    after_compaction(&mut progress, TRIGGER - MIN_ROOM_TO_WORK + 1);
    assert_eq!(step(&mut progress, TRIGGER), CompactionStep::Compact, "one isn't a pattern");
    after_compaction(&mut progress, TRIGGER - MIN_ROOM_TO_WORK + 1);
    assert_eq!(step(&mut progress, TRIGGER), CompactionStep::Go);
    assert_eq!(step(&mut progress, LIMIT - 1), CompactionStep::Go);
    assert_eq!(step(&mut progress, LIMIT), CompactionStep::Compact);
    assert!(progress.room_only());
}

#[test]
fn test_exactly_enough_room_isnt_tight_and_clears_what_was_learned() {
    let mut progress = CompactionProgress::default();
    for _ in 0..2 {
        after_compaction(&mut progress, TRIGGER);
    }
    assert!(progress.tight);
    after_compaction(&mut progress, TRIGGER - MIN_ROOM_TO_WORK);
    assert_eq!(step(&mut progress, TRIGGER), CompactionStep::Compact);
    assert!(!progress.room_only());
    after_compaction(&mut progress, TRIGGER);
    assert_eq!(step(&mut progress, TRIGGER), CompactionStep::Compact, "the count started again");
}

#[test]
fn test_the_third_result_in_a_row_near_the_limit_is_stuck() {
    let mut progress = CompactionProgress::default();
    let near = LIMIT - SHORT_REPLY_TOKENS + 1;
    for _ in 0..2 {
        after_compaction(&mut progress, near);
        assert_eq!(step(&mut progress, LIMIT), CompactionStep::Compact);
    }
    after_compaction(&mut progress, near);
    let CompactionStep::Stuck(why) = step(&mut progress, 0) else {
        panic!("the conversation doesn't fit");
    };
    assert!(why.contains("(100,000 tokens) is too small") && why.contains("about 91,809 tokens"), "{why}");
}

#[test]
fn test_a_result_with_room_under_the_limit_starts_that_count_again() {
    let mut progress = CompactionProgress::default();
    let near = LIMIT - SHORT_REPLY_TOKENS + 1;
    for _ in 0..2 {
        after_compaction(&mut progress, near);
    }
    after_compaction(&mut progress, LIMIT - SHORT_REPLY_TOKENS);
    for _ in 0..2 {
        after_compaction(&mut progress, near);
    }
    assert_eq!(step(&mut progress, 0), CompactionStep::Go);
}

#[test]
fn test_the_user_writing_tries_again_one_short_of_stuck() {
    let mut progress = CompactionProgress::default();
    for _ in 0..3 {
        after_compaction(&mut progress, LIMIT);
    }
    assert!(matches!(step(&mut progress, 0), CompactionStep::Stuck(_)));
    progress.user_wrote();
    assert_eq!(step(&mut progress, LIMIT), CompactionStep::Compact);
    assert!(progress.tight, "what's known of the room stays");
    after_compaction(&mut progress, LIMIT);
    assert!(matches!(step(&mut progress, 0), CompactionStep::Stuck(_)));
}

#[test]
fn test_a_new_window_forgets_everything() {
    let mut progress = CompactionProgress::default();
    for _ in 0..3 {
        after_compaction(&mut progress, LIMIT);
    }
    assert_eq!(progress.step("m", 500_000, Some(LIMIT)), CompactionStep::Go);
    assert!(!progress.room_only());
    // A compaction awaiting its measure, when a window is learned: the
    // measure is against nothing that was asked of it.
    progress.compacted(500_000);
    progress.measured(&input(LIMIT), 90_000);
    assert_eq!(progress.after, None);
}

#[test]
fn test_nothing_is_learned_or_failed_against_a_guess() {
    let mut progress = CompactionProgress::default();
    progress.guessed(WINDOW, 40_000);
    for _ in 0..4 {
        progress.compacted(40_000);
        progress.measured(&input(39_000), 40_000);
    }
    assert_eq!(progress.step("m", 40_000, Some(39_000)), CompactionStep::Go, "the server says when it doesn't fit");
    assert!(!progress.room_only(), "and how much reply fits isn't known");
    // The next turn believes the real window again, and nothing carries.
    assert_eq!(step(&mut progress, TRIGGER - 1), CompactionStep::Go);
    assert_eq!(progress.after, None);
    assert_eq!(step(&mut progress, TRIGGER), CompactionStep::Compact);
}

#[test]
fn test_a_guess_compacts_at_its_limit_not_its_trigger() {
    let mut progress = CompactionProgress::default();
    progress.guessed(WINDOW, 40_000);
    assert_eq!(progress.step("m", 40_000, Some(33_855)), CompactionStep::Go);
    assert_eq!(progress.step("m", 40_000, Some(33_856)), CompactionStep::Compact);
    progress.compacted(40_000);
    progress.measured(&input(10_000), 40_000);
    assert_eq!(progress.step("m", 40_000, Some(33_855)), CompactionStep::Go);
}

#[test]
fn test_a_real_window_replacing_a_guess_starts_afresh() {
    let mut progress = CompactionProgress::default();
    progress.guessed(WINDOW, 40_000);
    assert_eq!(progress.step("m", 60_000, Some(43_616)), CompactionStep::Compact);
    assert_eq!(progress.guess, None);
}

#[test]
fn test_the_fourth_refusal_in_a_row_ends_it_and_a_step_starts_the_count_again() {
    let mut progress = CompactionProgress::default();
    for _ in 0..3 {
        assert_eq!(progress.why_refused_out("m", WINDOW), None);
        progress.refusal_compacted(WINDOW);
    }
    progress.step_taken();
    assert_eq!(progress.why_refused_out("m", WINDOW), None);
    for _ in 0..3 {
        progress.refusal_compacted(WINDOW);
    }
    let why = progress.why_refused_out("m", WINDOW).expect("three in a row");
    assert!(why.contains("kept refusing") && !why.contains("100,000"), "{why}");
    assert!(matches!(step(&mut progress, 0), CompactionStep::Stuck(_)), "a wake doesn't start it over");
    progress.user_wrote();
    assert_eq!(step(&mut progress, 0), CompactionStep::Go);
    progress.refusal_compacted(WINDOW);
    assert!(progress.why_refused_out("m", WINDOW).is_some(), "a user's retry gets one more");
}

fn tight() -> CompactionProgress {
    let mut progress = CompactionProgress::default();
    for _ in 0..2 {
        after_compaction(&mut progress, 80_000);
    }
    assert!(progress.room_only());
    progress
}

#[test]
fn test_a_reply_the_room_cut_off_compacts_next_whatever_its_size_then_once_only() {
    let mut progress = tight();
    assert!(progress.cut_for_room(Some(90_000), WINDOW));
    assert_eq!(step(&mut progress, 0), CompactionStep::Compact);
    assert_eq!(step(&mut progress, 0), CompactionStep::Go, "the one compaction");
    assert!(!progress.cut_for_room(Some(90_000), WINDOW), "a second cut in a row ends the turn");
    progress.reply_finished();
    assert!(progress.cut_for_room(Some(90_000), WINDOW), "a reply finished between");
    let mut progress = tight();
    assert!(progress.cut_for_room(Some(90_000), WINDOW));
    progress.user_wrote();
    assert!(progress.cut_for_room(Some(90_000), WINDOW), "the user's send too");
}

#[test]
fn test_a_cut_is_only_answered_when_compacting_would_buy_room() {
    assert!(!tight().cut_for_room(None, WINDOW), "straight after a compaction");
    assert!(!tight().cut_for_room(Some(82_048), WINDOW), "exactly a short reply's worth isn't more");
    assert!(tight().cut_for_room(Some(82_049), WINDOW));
    let mut untight = CompactionProgress::default();
    after_compaction(&mut untight, 10_000);
    assert!(!untight.cut_for_room(Some(90_000), WINDOW), "the usual trigger kept the reply its room");
    let mut guessing = tight();
    guessing.guessed(WINDOW, 40_000);
    assert!(!guessing.cut_for_room(Some(39_000), 40_000));
    assert!(!tight().cut_for_room(Some(90_000), 500_000), "a window learned since");
}

#[test]
fn test_the_floor_is_stuck_and_names_what_was_measured_not_the_estimate() {
    let mut progress = tight();
    after_compaction(&mut progress, 96_000);
    let FloorAnswer::NoRoom(why) = progress.floor_fired(99_000, "m", WINDOW) else {
        panic!("no room by either number");
    };
    assert!(why.contains("(100,000 tokens) is too small") && why.contains("about 96,000 tokens"), "{why}");
    assert!(progress.is_stuck());
    assert_eq!(progress.why_if_stuck("m", WINDOW).as_deref(), Some(why.as_str()));
    progress.user_wrote();
    assert_eq!(progress.why_if_stuck("m", WINDOW), None);
}

#[test]
fn test_what_was_measured_outweighs_the_estimate_when_it_shows_room() {
    let mut progress = tight();
    assert_eq!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::Believe(80_000));
    assert!(!progress.is_stuck());
    // Exactly the buffer and the floor, then one token over.
    after_compaction(&mut progress, 94_880);
    assert_eq!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::Believe(94_880));
    after_compaction(&mut progress, 94_881);
    assert!(matches!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::NoRoom(_)));
}

#[test]
fn test_with_nothing_measured_the_estimate_is_the_number_to_name() {
    let mut progress = CompactionProgress::default();
    let FloorAnswer::NoRoom(why) = progress.floor_fired(99_000, "m", WINDOW) else {
        panic!("no room");
    };
    assert!(why.contains("about 99,000 tokens"), "{why}");
}

#[test]
fn test_the_users_send_after_stuck_lets_one_request_through() {
    let mut progress = tight();
    after_compaction(&mut progress, 96_000);
    assert!(matches!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::NoRoom(_)));
    progress.user_wrote();
    assert_eq!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::TryAnyway);
    assert!(matches!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::NoRoom(_)), "one");
    progress.user_wrote();
    progress.step_taken();
    assert!(matches!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::NoRoom(_)), "a step taken uses it up");
}

#[test]
fn test_a_send_whose_turn_never_got_to_the_floor_keeps_its_try() {
    let mut progress = tight();
    after_compaction(&mut progress, 96_000);
    assert!(matches!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::NoRoom(_)));
    progress.user_wrote();
    progress.user_wrote();
    assert_eq!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::TryAnyway);
}

#[test]
fn test_the_users_send_drops_a_compaction_owed_for_the_room() {
    let mut progress = tight();
    assert!(progress.cut_for_room(Some(90_000), WINDOW));
    progress.user_wrote();
    assert_eq!(step(&mut progress, 0), CompactionStep::Go);
}

#[test]
fn test_only_the_first_reply_after_a_compaction_measures_it() {
    let mut progress = CompactionProgress::default();
    after_compaction(&mut progress, 10_000);
    progress.measured(&input(90_000), WINDOW);
    assert_eq!(progress.after, Some(10_000), "later replies grow with the turn's own work");
}

#[test]
fn test_a_server_reporting_no_usage_measures_nothing() {
    let mut progress = CompactionProgress::default();
    after_compaction(&mut progress, 0);
    assert_eq!(progress.after, None);
    progress.measured(&input(90_000), WINDOW);
    assert_eq!(progress.after, Some(90_000), "the compaction was still waiting for a measurement");
}

#[test]
fn test_cache_tokens_are_input_and_output_isnt() {
    let mut progress = CompactionProgress::default();
    progress.compacted(WINDOW);
    progress.measured(
        &anthropic::TokenUsage {
            input_tokens: 10,
            output_tokens: 90_000,
            cache_creation_input_tokens: 20,
            cache_read_input_tokens: 30,
        },
        WINDOW,
    );
    assert_eq!(progress.after, Some(60));
}

/// The loop's own shape (SME-414): one accepted step between each
/// compaction. The opening compaction has no step before it and counts
/// neither way.
fn thrash(progress: &mut CompactionProgress, n: u32) {
    progress.compacted(WINDOW);
    for _ in 0..n {
        progress.step_taken();
        progress.compacted(WINDOW);
    }
}

#[test]
fn test_one_step_between_compactions_counts_the_loop_and_two_clear_it() {
    let mut progress = CompactionProgress::default();
    thrash(&mut progress, 3);
    assert_eq!(progress.thrash, 3, "one step between each");
    progress.step_taken();
    progress.compacted(WINDOW);
    assert_eq!(progress.thrash, 4, "the count goes on while the shape does");
    progress.step_taken();
    progress.step_taken();
    progress.compacted(WINDOW);
    assert_eq!(progress.thrash, 0, "two worked steps mean the room was used");
    progress.step_taken();
    progress.compacted(WINDOW);
    assert_eq!(progress.thrash, 1, "the run starts over from there");
    thrash(&mut progress, 2);
    progress.compacted(WINDOW);
    assert_eq!(progress.thrash, 3, "no step between: MAX_REFUSAL_COMPACTIONS' job, not this one");
}

#[test]
fn test_the_notice_fires_once_at_its_number_and_the_turn_ends_at_end_at() {
    let mut progress = CompactionProgress::default();
    thrash(&mut progress, NOTICE_AT - 1);
    assert_eq!(progress.take_thrash_notice(), None, "not yet");
    progress.step_taken();
    progress.compacted(WINDOW);
    let notice = progress.take_thrash_notice().expect("told at NOTICE_AT");
    assert!(notice.contains("bigger than the room a compaction leaves"), "{notice}");
    assert_eq!(progress.take_thrash_notice(), None, "once only");
    // The loop goes on being counted to the end, and the next step ends
    // the turn and arms the stop.
    for _ in 0..(END_AT - NOTICE_AT) {
        progress.step_taken();
        progress.compacted(WINDOW);
    }
    let CompactionStep::Stuck(why) = step(&mut progress, TRIGGER) else { panic!("it ends the turn") };
    assert!(why.contains("one step between each"), "{why}");
    assert!(why.contains(&END_AT.to_string()), "{why}");
    assert!(progress.is_stuck(), "and the conversation is stopped");
    // Armed: every later step says so before the call, on the number-free
    // wording so `already_said` dedupes a wake's repeats.
    let CompactionStep::Stuck(armed) = step(&mut progress, TRIGGER) else { panic!("still stopped") };
    assert!(armed.contains("compacting after every step"), "{armed}");
    assert!(!armed.chars().any(|c| c.is_ascii_digit()), "number-free: {armed}");
}

#[test]
fn test_the_users_send_clears_the_loop_fully() {
    let mut progress = CompactionProgress::default();
    thrash(&mut progress, END_AT);
    assert!(matches!(step(&mut progress, TRIGGER), CompactionStep::Stuck(_)));
    progress.user_wrote();
    assert!(!progress.is_stuck());
    assert_eq!(step(&mut progress, TRIGGER - 1), CompactionStep::Go, "fresh runway");
    // And the notice can fire again: the send got a whole new count.
    thrash(&mut progress, NOTICE_AT);
    assert!(progress.take_thrash_notice().is_some());
}

#[test]
fn test_a_queued_message_lifts_the_stop_and_only_the_stop() {
    let mut progress = CompactionProgress::default();
    thrash(&mut progress, END_AT);
    assert!(matches!(step(&mut progress, TRIGGER), CompactionStep::Stuck(_)));
    progress.clear_thrash();
    assert!(!progress.is_stuck(), "the armed stop lifts");
    assert_eq!(step(&mut progress, TRIGGER), CompactionStep::Compact, "the next compaction goes, and it reads the message");
    // `stuck` is another matter: sizes are not what a message changes.
    let mut progress = CompactionProgress::default();
    progress.too_small(99_000, WINDOW);
    progress.clear_thrash();
    assert!(progress.is_stuck(), "still out of room");
}

#[test]
fn test_nothing_is_counted_or_fired_against_a_guess() {
    let mut progress = CompactionProgress::default();
    let guess = 50_000;
    progress.guessed(WINDOW, guess);
    for _ in 0..END_AT + 1 {
        progress.step_taken();
        progress.compacted(guess);
    }
    assert_eq!(progress.thrash, 0, "nothing is counted against a guess");
    assert_eq!(progress.take_thrash_notice(), None);
    assert!(!progress.is_stuck());
}

#[test]
fn test_a_new_window_clears_the_armed_stop() {
    let mut progress = CompactionProgress::default();
    thrash(&mut progress, END_AT);
    assert!(matches!(step(&mut progress, TRIGGER), CompactionStep::Stuck(_)));
    // The provider page's bigger window: what was counted was of the old one.
    assert_eq!(progress.step("m", 200_000, Some(TRIGGER)), CompactionStep::Go);
    assert!(!progress.is_stuck());
}

/// T6 (SME-482): the model's once a turn survives a window learned mid-turn,
/// and so does a request not yet taken; the next turn's start clears both.
#[test]
fn test_the_models_compaction_stays_spent_across_a_window_change() {
    let mut progress = CompactionProgress::default();
    progress.sync(WINDOW);
    assert_eq!(progress.request(Some("n".to_string())), Ok(()));
    assert!(progress.request(None).expect_err("asked twice").contains("already asked for in this reply"));
    assert_eq!(progress.take_request(), Some(Some("n".to_string())));
    assert_eq!(progress.take_request(), None, "taken once");
    progress.sync(WINDOW / 2);
    assert!(progress.request(None).expect_err("still spent").contains("once this turn"));
    progress.turn_started();
    assert_eq!(progress.request(None), Ok(()));
    progress.sync(WINDOW);
    assert_eq!(progress.take_request(), Some(None), "pending survives a window change");
}

/// SME-423: all three stuck wordings — the measured floor, the window
/// too small for the prompt alone, and the server that keeps refusing —
/// send the user to both places a window can be raised.
#[test]
fn test_every_stuck_wording_names_the_providers_page() {
    let mut progress = tight();
    after_compaction(&mut progress, 96_000);
    let FloorAnswer::NoRoom(why) = progress.floor_fired(99_000, "m", WINDOW) else {
        panic!("no room by either number");
    };
    assert!(why.ends_with(PROVIDERS_PAGE_TAIL), "the floor wording: {why}");
    let mut progress = CompactionProgress::default();
    progress.too_small(99_000, WINDOW);
    let why = progress.why_if_stuck("m", WINDOW).expect("stuck");
    assert!(why.ends_with(PROVIDERS_PAGE_TAIL), "the too-small wording: {why}");
    let mut progress = CompactionProgress::default();
    for _ in 0..3 {
        progress.refusal_compacted(WINDOW);
    }
    let why = progress.why_refused_out("m", WINDOW).expect("refused out");
    assert!(why.ends_with(PROVIDERS_PAGE_TAIL), "the refused-out wording: {why}");
}

/// SME-423 C: a send that ended in something other than a refusal proves
/// nothing about the measured `after`, and the size believed for it must
/// not go on to make a later refusal look like that size was disproved.
#[test]
fn test_a_failed_send_stops_believing_the_measured_size() {
    let mut progress = tight();
    assert_eq!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::Believe(80_000), "the request out is sized from what was measured");
    progress.request_failed();
    progress.refused(Some(120_000));
    assert_eq!(progress.after, Some(80_000), "a refusal of a request no longer believed says nothing of the size");
    assert_eq!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::Believe(80_000), "the floor believes again for the request now out");
    progress.refused(Some(120_000));
    assert_eq!(progress.after, Some(120_000), "a refusal straight after a believed send still shows the size out of date");
}
