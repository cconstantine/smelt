//! The subscription's usage windows (SME-486): parsed from the S5/S6
//! captures' real `rate_limit_event`s, checked so no string of the event
//! survives, and paced from now. Every fixture pins `resetsAt` to one
//! value, so the pace tests synthesize their events (marked so).

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{Value, json};

use crate::claude_cli::usage::*;

fn events(fixture: &str) -> Vec<Value> {
    fixture
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("a fixture line"))
        .filter(|event| event["type"] == "rate_limit_event")
        .collect()
}

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("a time")
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[test]
fn test_a_fixture_streams_windows_parse_to_numbers() {
    let capped = events(include_str!("../fixtures/capped.jsonl"));
    let looped = events(include_str!("../fixtures/loop.jsonl"));
    assert_eq!(capped.len(), 1, "capped.jsonl has one event");
    // The capture holds one event (the plan expected one per API call).
    assert_eq!(looped.len(), 1, "loop.jsonl has one event");
    let seen_at = at(1_767_220_000);
    for event in capped.iter().chain(&looped) {
        let reading = Reading::from_event(event, seen_at).expect("a reading");
        assert_eq!(reading.seen_at, seen_at);
        assert_eq!(
            reading.windows,
            vec![
                Window { name: "five_hour".into(), utilization: 0.09, status: "allowed".into(), resets_at: Some(at(1_767_225_600)) },
                Window { name: "seven_day".into(), utilization: 0.32, status: "allowed".into(), resets_at: Some(at(1_767_225_600)) },
            ]
        );
    }
    // The pending slot: the run's last event wins, and taking it empties
    // it. Three events synthesized from the capture's one, numbers changed.
    let conversation_id = -486_001;
    for i in 0..3 {
        let mut event = looped[0].clone();
        event["rate_limit_info"]["unifiedWindows"]["five_hour"]["utilization"] = json!(0.1 + i as f64 / 100.0);
        seen(conversation_id, &event);
    }
    let last = take(conversation_id).expect("a pending reading");
    assert!(close(last.windows[0].utilization, 0.12), "{last:?}");
    assert_eq!(take(conversation_id), None, "taken");
    // No unifiedWindows, or none valid: no reading, so the last one stands.
    assert_eq!(Reading::from_event(&json!({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed"}}), seen_at), None);
    seen(conversation_id, &json!({"type": "rate_limit_event", "rate_limit_info": {"unifiedWindows": {"BAD": {"utilization": 0.5}}}}));
    assert_eq!(take(conversation_id), None, "an invalid event leaves the slot as it was");
}

/// Round 1 A-L1: the held map can't keep an entry for good. A reading held
/// longer than `HELD_AT_MOST` goes when any other is held, and `discard`
/// drops one.
#[test]
fn test_a_held_reading_is_swept_once_old() {
    let event = &events(include_str!("../fixtures/capped.jsonl"))[0];
    let (old, kept, fresh) = (-486_002, -486_003, -486_004);
    let reading = |seen_at| Reading::from_event(event, seen_at).expect("a reading");
    hold(old, reading(Utc::now() - HELD_AT_MOST - Duration::minutes(1)));
    hold(kept, reading(Utc::now() - HELD_AT_MOST + Duration::minutes(5)));
    hold(fresh, reading(Utc::now()));
    assert_eq!(take(old), None, "swept");
    assert!(take(kept).is_some(), "younger than the limit stays");
    discard(fresh);
    assert_eq!(take(fresh), None, "discarded");
}

/// Every string an event can carry, planted with one marker.
pub(crate) const MARKER: &str = "SK_ANT_MARKER_486";

/// A `rate_limit_event` with `MARKER` in every string it can carry, plus
/// one real window.
pub(crate) fn planted_event() -> Value {
    json!({
        "type": "rate_limit_event",
        "session_id": format!("{MARKER}-session"),
        "uuid": format!("{MARKER}-uuid"),
        "rate_limit_info": {
            "status": "allowed",
            "rateLimitType": format!("{MARKER}-type"),
            "resetsAt": 1_767_225_600,
            "overageDisabledReason": format!("{MARKER}-reason"),
            "unifiedWindows": {
                "five_hour": {"utilization": 0.45, "status": format!("sk-ant-{MARKER}"), "resetsAt": 1_767_225_600},
                MARKER: {"utilization": 0.5, "status": "allowed", "resetsAt": 1_767_225_600},
                format!("sk-ant-{}", MARKER.to_lowercase()): {"utilization": 0.5, "status": "allowed"},
                "seven_day": {"utilization": format!("{MARKER}"), "status": "allowed", "resetsAt": format!("{MARKER}")},
            },
        },
    })
}

#[test]
fn test_a_reading_holds_no_string_from_the_event() {
    let reading = Reading::from_event(&planted_event(), at(1_767_220_000)).expect("a reading");
    let shown = format!("{} {reading:?}", serde_json::to_string(&reading).expect("serialize"));
    assert!(!shown.to_lowercase().contains(&MARKER.to_lowercase()), "{shown}");
    assert_eq!(reading.windows.len(), 1, "only five_hour is valid: {reading:?}");
    assert_eq!(reading.windows[0].status, "unknown", "a status outside the list is unknown");
    // A number out of range, or not finite, drops its window.
    for bad in [json!(-0.1), json!(10.5), json!("0.5"), json!(null)] {
        let event = json!({"rate_limit_info": {"unifiedWindows": {"five_hour": {"utilization": bad, "status": "allowed"}}}});
        assert_eq!(Reading::from_event(&event, at(0)), None, "{bad}");
    }
    // A reset outside 2020..2100 is no reset.
    let event = json!({"rate_limit_info": {"unifiedWindows": {"five_hour": {"utilization": 1.2, "status": "rejected", "resetsAt": 99}}}});
    let reading = Reading::from_event(&event, at(0)).expect("a reading");
    assert_eq!(reading.windows[0].resets_at, None);
    assert_eq!(reading.windows[0].status, "rejected");
    assert!(close(reading.windows[0].utilization, 1.2), "overage can pass 1.0");
}

#[test]
fn test_pace_and_reset_from_now() {
    // Synthesized windows: the fixtures' resetsAt is one fixed value.
    let now = at(1_767_225_600);
    // Read just now: the pace is as of the reading (round 1 A-L3).
    let seen_at = now;
    let window = |name: &str, utilization: f64, left: Option<Duration>| {
        UsageWindow::from_stored(name.into(), utilization, "allowed".into(), left.map(|left| now + left), seen_at, now)
    };
    let half = window("seven_day", 0.32, Some(Duration::hours(84)));
    assert!(close(half.elapsed_fraction.expect("elapsed"), 0.5), "{half:?}");
    assert!(close(half.projected.expect("projected"), 0.64), "{half:?}");
    assert!(!half.reset_passed);
    assert!(close(window("seven_day", 0.60, Some(Duration::hours(84))).projected.expect("projected"), 1.2));
    let first_day = window("seven_day", 0.10, Some(Duration::hours(156)));
    assert!(close(first_day.elapsed_fraction.expect("elapsed"), 1.0 / 14.0), "{first_day:?}");
    assert_eq!(first_day.projected, None, "no pace on a week's first day");
    let five = window("five_hour", 0.45, Some(Duration::minutes(150)));
    assert!(close(five.projected.expect("projected"), 0.9), "{five:?}");
    let passed = window("five_hour", 0.95, Some(Duration::minutes(-1)));
    assert!(passed.reset_passed);
    assert_eq!(passed.projected, None);
    assert_eq!(passed.elapsed_fraction, Some(1.0), "clamped");
    let other = window("seven_day_opus", 0.5, Some(Duration::hours(84)));
    assert_eq!((other.elapsed_fraction, other.projected), (None, None), "an unknown window has no length");
    let no_reset = window("seven_day", 0.5, None);
    assert_eq!((no_reset.elapsed_fraction, no_reset.projected, no_reset.reset_passed), (None, None, false));
    assert_eq!(half.seen_at, seen_at);
}

/// Round 1 A-L3: the pace is the reading's, as of when it was seen, not
/// the answer's time: a week half gone when 32% was read is on pace for
/// 64%, however long ago that was. Whether the reset has passed is still
/// as of now.
#[test]
fn test_the_pace_is_as_of_the_reading() {
    let seen_at = at(1_767_225_600);
    let resets_at = seen_at + Duration::hours(84);
    let window = |now| UsageWindow::from_stored("seven_day".into(), 0.32, "allowed".into(), Some(resets_at), seen_at, now);
    let later = window(seen_at + Duration::hours(48));
    assert!(close(later.elapsed_fraction.expect("elapsed"), 0.5), "{later:?}");
    assert!(close(later.projected.expect("projected"), 0.64), "{later:?}");
    assert!(!later.reset_passed);
    let after = window(resets_at + Duration::minutes(1));
    assert!(after.reset_passed);
    assert_eq!(after.projected, None, "an ended window has no pace");
}
