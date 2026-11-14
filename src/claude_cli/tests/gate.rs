//! The five-hour usage gate (SME-523): its verdict over a table (G1), the
//! release order (G5) and the newest reading winning (G6), on gates of
//! their own; the turn path's tests are in `turn::tests::claude_cli::gate`.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};

use crate::claude_cli::gate::{FiveHour, Gate, LINE, Verdict, WINDOW, verdict};
use crate::claude_cli::usage::{Reading, Window};

/// A reading of `utilization` at `elapsed` of the window gone by, as of `now`.
fn at(utilization: f64, elapsed: f64, now: DateTime<Utc>) -> FiveHour {
    let left = Duration::milliseconds(((1.0 - elapsed) * WINDOW.num_milliseconds() as f64) as i64);
    FiveHour { utilization, status: "allowed".into(), resets_at: Some(now + left), seen_at: now }
}

fn until(v: Verdict) -> DateTime<Utc> {
    match v {
        Verdict::Held { until } => until,
        Verdict::Open => panic!("held, not open"),
    }
}

/// G1: the line is `u ≥ 0.95 × e`, with `e` floored at 1/7.
#[test]
fn test_the_verdict_holds_on_pace_to_pass_the_line() {
    let now = Utc::now();
    assert_eq!(verdict(Some(&at(0.28, 0.30, now)), now), Verdict::Open, "0.93 of the line");
    let held = at(0.29, 0.30, now);
    let release = until(verdict(Some(&held), now));
    let start = held.resets_at.expect("a reset") - WINDOW;
    let expected = start + Duration::milliseconds((WINDOW.num_milliseconds() as f64 * 0.29 / LINE) as i64);
    assert!((release - expected).num_milliseconds().abs() <= 1, "release at start + 5h × u / 0.95: {release} vs {expected}");
    assert!(release > now, "{release}");
    assert_eq!(verdict(Some(&held), release), Verdict::Open, "open once the release comes");
    assert!(matches!(verdict(Some(&held), release - Duration::seconds(1)), Verdict::Held { .. }), "held just before");
}

#[test]
fn test_the_verdict_floors_the_elapsed_share_at_a_seventh() {
    let now = Utc::now();
    assert!(matches!(verdict(Some(&at(0.14, 0.05, now)), now), Verdict::Held { .. }), "0.14 ÷ 1/7 = 0.98");
    assert_eq!(verdict(Some(&at(0.13, 0.05, now)), now), Verdict::Open, "0.13 ÷ 1/7 = 0.91, not 0.13 ÷ 0.05");
}

#[test]
fn test_a_rejected_or_full_window_is_held_until_its_reset() {
    let now = Utc::now();
    let mut rejected = at(0.50, 0.30, now);
    rejected.status = "rejected".into();
    assert_eq!(verdict(Some(&rejected), now), Verdict::Held { until: rejected.resets_at.expect("a reset") });
    let full = at(0.97, 0.90, now);
    assert_eq!(verdict(Some(&full), now), Verdict::Held { until: full.resets_at.expect("a reset") }, "never past the reset");
}

#[test]
fn test_the_verdict_fails_open() {
    let now = Utc::now();
    assert_eq!(verdict(None, now), Verdict::Open, "no reading");
    let mut passed = at(1.0, 0.5, now - WINDOW);
    passed.status = "rejected".into();
    assert_eq!(verdict(Some(&passed), now), Verdict::Open, "a reset passed");
    let mut no_reset = at(1.0, 0.5, now);
    no_reset.resets_at = None;
    assert_eq!(verdict(Some(&no_reset), now), Verdict::Open, "no reset time");
}

#[test]
fn test_the_gate_reads_a_readings_five_hour_window() {
    let now = Utc::now();
    let resets = Some(now + Duration::hours(1));
    let window = |name: &str, utilization| Window { name: name.into(), utilization, status: "allowed".into(), resets_at: resets };
    let reading = Reading { windows: vec![window("seven_day", 0.40), window("five_hour", 0.20)], seen_at: now };
    assert_eq!(
        FiveHour::from_reading(&reading),
        Some(FiveHour { utilization: 0.20, status: "allowed".into(), resets_at: resets, seen_at: now })
    );
    let seven_only = Reading { windows: vec![window("seven_day", 0.40)], seen_at: now };
    assert_eq!(FiveHour::from_reading(&seven_only), None);
}

/// G6: a late, older reading doesn't reopen a hold.
#[test]
fn test_the_newest_reading_wins() {
    let gate = Gate::new(StdDuration::ZERO);
    let now = Utc::now();
    gate.observe(at(0.99, 0.5, now));
    assert!(matches!(gate.verdict(), Verdict::Held { .. }));
    gate.observe(at(0.01, 0.5, now - Duration::seconds(5)));
    assert!(matches!(gate.verdict(), Verdict::Held { .. }), "the older reading is ignored");
    gate.observe(at(0.01, 0.5, now + Duration::milliseconds(1)));
    assert_eq!(gate.verdict(), Verdict::Open, "a newer one is taken");
}

async fn until_waiting(gate: &Gate, n: usize) {
    for _ in 0..500 {
        if gate.waiting().len() == n {
            return;
        }
        tokio::time::sleep(StdDuration::from_millis(5)).await;
    }
    panic!("{n} waiters never queued: {:?}", gate.waiting());
}

/// G5: warm waiters first, oldest first among each, a spacing apart. With
/// FIFO alone the cold, older waiter would go first.
#[tokio::test]
async fn test_waiters_pass_warm_first_then_oldest_a_spacing_apart() {
    let spacing = StdDuration::from_millis(50);
    let gate = Arc::new(Gate::new(spacing));
    let (warm_old, cold_older, warm_new) = (1, 2, 3);
    // Open and no one queued: each passes at once, and is warm after.
    for id in [warm_old, warm_new] {
        tokio::time::timeout(StdDuration::from_secs(1), gate.wait(id, |_| {})).await.expect("passes at once");
    }
    let now = Utc::now();
    gate.observe(at(0.99, 0.5, now));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    for (n, id) in [cold_older, warm_old, warm_new].into_iter().enumerate() {
        let (shared, tx) = (gate.clone(), tx.clone());
        tokio::spawn(async move {
            shared.wait(id, |_| {}).await;
            let _ = tx.send((id, tokio::time::Instant::now()));
        });
        until_waiting(&gate, n + 1).await;
    }
    gate.observe(at(0.01, 0.5, now + Duration::milliseconds(1)));
    let mut passed = Vec::new();
    for _ in 0..3 {
        passed.push(tokio::time::timeout(StdDuration::from_secs(5), rx.recv()).await.expect("each passes").expect("sent"));
    }
    let order: Vec<i64> = passed.iter().map(|(id, _)| *id).collect();
    assert_eq!(order, vec![warm_old, warm_new, cold_older]);
    for pair in passed.windows(2) {
        let gap = pair[1].1 - pair[0].1;
        assert!(gap >= spacing - StdDuration::from_millis(10), "a spacing apart: {gap:?}");
    }
    assert!(gate.waiting().is_empty());
}

/// A dropped waiter leaves the queue at once, and the next can go.
#[tokio::test]
async fn test_a_dropped_waiter_leaves_the_queue() {
    let gate = Arc::new(Gate::new(StdDuration::ZERO));
    let now = Utc::now();
    gate.observe(at(0.99, 0.5, now));
    let waiting = tokio::spawn({
        let gate = gate.clone();
        async move { gate.wait(7, |_| {}).await }
    });
    until_waiting(&gate, 1).await;
    waiting.abort();
    let _ = waiting.await;
    assert!(gate.waiting().is_empty(), "{:?}", gate.waiting());
}
