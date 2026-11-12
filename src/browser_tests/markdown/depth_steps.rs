//! The per-depth driver of `scenario_markdown_deep_code` and its step
//! record (SME-500). Each depth's awaits run as named steps, recorded
//! outside the depth's timed future, so a depth that stalls still says
//! which step it stalled in and how its steps went until then.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;

/// How one step ended.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Outcome {
    Running,
    Ok,
    /// A poll reached its own bound and gave up; the depth carries on.
    GaveUp(Duration),
    /// The depth's shared deadline came first; the depth ends here.
    AtDeadline,
    Error(String),
    /// The depth ended before this step began.
    NotReached,
}

/// What a poll does when an evaluate fails.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum OnError {
    /// Count it and poll again within the same bound, as `wait_for_text`.
    Retry,
    /// End the step at once with the error, as `wait_for_live_client`,
    /// which panicked on the first one.
    Stop,
}

/// A polling step's counts: how many evaluates, the slowest one, how many
/// failed and the last failure's text, and when the one still in flight
/// started (from the depth's start).
#[derive(Clone, Debug, Default)]
pub(super) struct PollStats {
    pub(super) polls: u32,
    pub(super) max_eval: Duration,
    pub(super) eval_errors: u32,
    last_error: Option<String>,
    in_flight: Option<Duration>,
}

#[derive(Clone, Debug)]
pub(super) struct Step {
    pub(super) name: &'static str,
    /// When the step began, from the depth's start.
    pub(super) start: Duration,
    pub(super) elapsed: Duration,
    pub(super) outcome: Outcome,
    pub(super) stats: Option<PollStats>,
}

/// One depth's steps, in order, and its time limit. Made by the caller
/// before the depth's future, and read after it, however that ended.
pub(super) struct DepthSteps {
    names: &'static [&'static str],
    started: Instant,
    limit: Duration,
    steps: RefCell<Vec<Step>>,
    backstop_fired: Cell<bool>,
}

impl DepthSteps {
    /// The depth starts now, and its deadline is `limit` from now.
    pub(super) fn new(names: &'static [&'static str], limit: Duration) -> Self {
        DepthSteps { names, started: Instant::now(), limit, steps: RefCell::new(Vec::new()), backstop_fired: Cell::new(false) }
    }

    pub(super) fn deadline(&self) -> Instant {
        self.started + self.limit
    }

    /// Whether the depth's outer timeout fired, rather than its body ending.
    pub(super) fn backstop_fired(&self) -> bool {
        self.backstop_fired.get()
    }

    /// Every step so far, in order.
    pub(super) fn steps(&self) -> Vec<Step> {
        self.steps.borrow().clone()
    }

    /// The step that stopped at the depth's deadline, if one did.
    pub(super) fn stopped_at_deadline(&self) -> Option<Step> {
        self.steps.borrow().iter().find(|s| s.outcome == Outcome::AtDeadline).cloned()
    }

    /// The error step `name` ended with, if it ended with one.
    pub(super) fn error_in(&self, name: &str) -> Option<String> {
        self.steps.borrow().iter().find_map(|s| match &s.outcome {
            Outcome::Error(e) if s.name == name => Some(e.clone()),
            _ => None,
        })
    }

    /// Records every step the depth never began as not reached.
    pub(super) fn finish(&self) {
        let mut steps = self.steps.borrow_mut();
        for name in self.names {
            if !steps.iter().any(|s| s.name == *name) {
                steps.push(Step {
                    name,
                    start: Duration::ZERO,
                    elapsed: Duration::ZERO,
                    outcome: Outcome::NotReached,
                    stats: None,
                });
            }
        }
    }

    /// Runs `fut` as step `name`, which gives back its value and how it
    /// ended. Every step shares the depth's one deadline: a step gets
    /// min(its own bound, the time left), never more. When the deadline
    /// comes first, the step is recorded as stopped there and this returns
    /// `None`, and the depth should end: its body then returns normally, so
    /// the stall names its step without the backstop.
    pub(super) async fn step<T>(&self, name: &'static str, fut: impl Future<Output = (T, Outcome)>) -> Option<T> {
        let begun = Instant::now();
        let index = {
            let mut steps = self.steps.borrow_mut();
            steps.push(Step {
                name,
                start: begun - self.started,
                elapsed: Duration::ZERO,
                outcome: Outcome::Running,
                stats: None,
            });
            steps.len() - 1
        };
        let ended = tokio::time::timeout_at(self.deadline(), fut).await;
        let mut steps = self.steps.borrow_mut();
        let step = &mut steps[index];
        step.elapsed = begun.elapsed();
        // An evaluate still in flight at the deadline stays in the record
        // (`describe` times it to the step's end): a single blocked
        // evaluate must not read like many small ones.
        match ended {
            Ok((value, outcome)) => {
                step.outcome = outcome;
                Some(value)
            }
            Err(_) => {
                step.outcome = Outcome::AtDeadline;
                None
            }
        }
    }

    /// Polls `eval` until it reads true, every `interval`, giving up once
    /// `own` has passed (checked between evaluates, as the harness's
    /// `wait_for_live_client` and `wait_for_text` do). Counts each evaluate
    /// into the current step, so a step that stalls mid-poll still shows
    /// them, and the one in flight. A failed evaluate ends the step at once
    /// with `OnError::Stop`, and is polled again with `OnError::Retry`.
    pub(super) async fn poll<F, Fut>(&self, own: Duration, interval: Duration, on_error: OnError, mut eval: F) -> (bool, Outcome)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<bool, String>>,
    {
        let poll_deadline = Instant::now() + own;
        loop {
            let begun = Instant::now();
            self.with_stats(|s| s.in_flight = Some(begun - self.started));
            let read = eval().await;
            let took = begun.elapsed();
            self.with_stats(|s| {
                s.in_flight = None;
                s.polls += 1;
                s.max_eval = s.max_eval.max(took);
                if let Err(e) = &read {
                    s.eval_errors += 1;
                    s.last_error = Some(e.clone());
                }
            });
            match read {
                Ok(true) => return (true, Outcome::Ok),
                Err(e) if on_error == OnError::Stop => return (false, Outcome::Error(e)),
                _ => {}
            }
            if Instant::now() >= poll_deadline {
                return (false, Outcome::GaveUp(own));
            }
            tokio::time::sleep(interval).await;
        }
    }

    fn with_stats(&self, f: impl FnOnce(&mut PollStats)) {
        if let Some(step) = self.steps.borrow_mut().last_mut() {
            f(step.stats.get_or_insert_with(PollStats::default));
        }
    }

    /// The steps on one line: `seed 0.1 s, tab 1.2 s, live 0.4 s (polls 3,
    /// max_eval 0.3 s), …`, each step's outcome named unless it was ok.
    pub(super) fn report(&self) -> String {
        let now = Instant::now();
        let steps = self.steps.borrow();
        let mut parts: Vec<String> = steps.iter().map(|s| describe(s, now - self.started)).collect();
        for name in self.names {
            if !steps.iter().any(|s| s.name == *name) {
                parts.push(format!("{name} not reached"));
            }
        }
        parts.join(", ")
    }

    /// Why the outer timeout fired, and the steps until then: the step it
    /// fired in, or the last step that finished before it.
    pub(super) fn timeout_report(&self) -> String {
        self.backstop_fired.set(true);
        let now = Instant::now() - self.started;
        let depth = format!("(depth {:.1} s)", self.limit.as_secs_f64());
        let at = match self.steps.borrow().last() {
            Some(s) if s.outcome == Outcome::Running => {
                format!("timed out in step {} after {:.1} s {depth}", s.name, (now - s.start).as_secs_f64())
            }
            Some(s) => format!("timed out after step {} finished {depth}", s.name),
            None => format!("timed out before any step {depth}"),
        };
        format!("{at}; steps: {}", self.report())
    }
}

/// One step for a report; `now` is from the depth's start.
fn describe(s: &Step, now: Duration) -> String {
    let secs = |d: Duration| format!("{:.1} s", d.as_secs_f64());
    let mut line = match &s.outcome {
        Outcome::Ok => format!("{} {}", s.name, secs(s.elapsed)),
        Outcome::Running => format!("{} {} running", s.name, secs(now - s.start)),
        Outcome::GaveUp(own) => format!("{} {} gave up after its own bound ({})", s.name, secs(s.elapsed), secs(*own)),
        Outcome::AtDeadline => format!("{} {} stopped at the depth deadline", s.name, secs(s.elapsed)),
        Outcome::Error(e) => format!("{} {} error {e}", s.name, secs(s.elapsed)),
        Outcome::NotReached => return format!("{} not reached", s.name),
    };
    if let Some(stats) = &s.stats {
        line += &format!(" (polls {}, max_eval {}", stats.polls, secs(stats.max_eval));
        if stats.eval_errors > 0 {
            line += &format!(", eval_errors {}", stats.eval_errors);
        }
        if let Some(e) = &stats.last_error {
            line += &format!(", last_error {e}");
        }
        if let Some(since) = stats.in_flight {
            if s.outcome == Outcome::Running {
                line += &format!(", eval in flight {}", secs(now.saturating_sub(since)));
            } else {
                line += &format!(", eval in flight {} at the deadline", secs((s.start + s.elapsed).saturating_sub(since)));
            }
        }
        line += ")";
    }
    line
}

/// A depth's timing line, as far as it got: `-` for a value it never
/// reached.
pub(super) fn timing_line(
    depth: usize,
    live_after: Option<Duration>,
    rendered_after: Option<Duration>,
    span_depth: &serde_json::Value,
    steps: &str,
) -> String {
    let ms = |d: Option<Duration>| d.map_or("-".to_string(), |d| d.as_millis().to_string());
    let span = if span_depth.is_null() { "-".to_string() } else { span_depth.to_string() };
    format!(
        "browser tier: markdown_deep_code: depth {depth}: live in {} ms, rendered in {} ms, span depth {span}; steps: {steps}",
        ms(live_after),
        ms(rendered_after)
    )
}

/// Runs one depth's `body` under the deadline `steps` was made with. A
/// depth that hangs or panics ends alone, so the rest still run; its
/// failures come back prefixed with the depth, and a hang or a panic names
/// its step. A step stopped at the shared deadline ends the body normally
/// and is named here; the outer timeout, at the same deadline, stays as the
/// backstop for a stall outside every step.
pub(super) async fn run_depth(depth: usize, steps: &DepthSteps, body: impl Future<Output = Vec<String>>) -> Vec<String> {
    let outcome =
        futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(tokio::time::timeout_at(steps.deadline(), body)))
            .await;
    let mut problems = match outcome {
        Ok(Ok(problems)) => problems,
        Ok(Err(_)) => vec![steps.timeout_report()],
        Err(panic) => vec![format!(
            "{}; steps: {}",
            panic.downcast_ref::<String>().cloned().unwrap_or_else(|| "panicked".to_string()),
            steps.report()
        )],
    };
    if let Some(stopped) = steps.stopped_at_deadline() {
        problems.push(format!(
            "stopped at the depth deadline in step {} after {:.1} s (depth {:.1} s); steps: {}",
            stopped.name,
            stopped.elapsed.as_secs_f64(),
            steps.limit.as_secs_f64(),
            steps.report()
        ));
    }
    problems.into_iter().map(|p| format!("depth {depth}: {p}")).collect()
}

// No browser, no database, paused time. `#[ignore]` is the browser tier's
// run marker: `browser_tests` builds only with `browser-test`, and the tier
// (CI's browser job, `scripts/browser-tier`) runs it with `--ignored`.
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::sleep;

    const NAMES: &[&str] = &["seed", "tab", "live", "text", "facts", "still_live"];
    const LIMIT: Duration = Duration::from_secs(45);

    async fn takes(d: Duration) -> ((), Outcome) {
        sleep(d).await;
        ((), Outcome::Ok)
    }

    /// A stall outside every step (here after `tab`) still fires the
    /// backstop, and its failure names the last step that finished and
    /// keeps the steps' times. Before SME-500 it read only "didn't finish
    /// within 45 s".
    #[tokio::test(start_paused = true)]
    #[ignore]
    async fn test_a_stall_between_steps_names_the_last_step_and_keeps_the_times() {
        let steps = DepthSteps::new(NAMES, LIMIT);
        let failures = run_depth(2_000, &steps, async {
            steps.step("seed", takes(Duration::from_millis(100))).await;
            steps.step("tab", takes(Duration::from_secs(2))).await;
            sleep(Duration::from_secs(60)).await;
            Vec::new()
        })
        .await;
        assert_eq!(failures.len(), 1, "{failures:?}");
        let failure = &failures[0];
        assert!(failure.starts_with("depth 2000: timed out after step tab finished (depth 45.0 s); steps: "), "{failure}");
        assert!(failure.contains("seed 0.1 s, tab 2.0 s, live not reached"), "{failure}");
        assert!(steps.backstop_fired());
    }

    async fn never() -> Result<bool, String> {
        Ok(false)
    }

    fn named(steps: &DepthSteps, name: &str) -> Step {
        steps.steps().into_iter().find(|s| s.name == name).unwrap_or_else(|| panic!("no step {name}: {:?}", steps.steps()))
    }

    /// A body shaped like `deep_code_at`'s: each step ends the depth when it
    /// returns `None`, and the rest are recorded as not reached.
    async fn body(steps: &DepthSteps, tab: Duration, live_own: Duration) -> Vec<String> {
        let mut problems = Vec::new();
        let _ = async {
            steps.step("seed", takes(Duration::from_millis(100))).await?;
            steps.step("tab", takes(tab)).await?;
            if !steps.step("live", steps.poll(live_own, Duration::from_millis(100), OnError::Stop, never)).await? {
                problems.push("1: the client never went live".to_string());
            }
            steps.step("text", steps.poll(Duration::from_secs(20), Duration::from_millis(200), OnError::Retry, || async { Ok(true) })).await?;
            steps.step("facts", takes(Duration::ZERO)).await?;
            steps.step("still_live", steps.poll(Duration::from_secs(10), Duration::from_millis(200), OnError::Retry, || async { Ok(true) })).await
        }
        .await;
        steps.finish();
        problems
    }

    /// A stall inside a step stops that step at the depth's shared deadline,
    /// which names it, and the depth ends through its body, not the
    /// backstop: `tab` takes 40 s, so `live` gets the 4.9 s left, not its 15.
    #[tokio::test(start_paused = true)]
    #[ignore]
    async fn test_a_stall_inside_a_step_stops_at_the_depth_deadline_and_names_it() {
        let steps = DepthSteps::new(NAMES, LIMIT);
        let failures = run_depth(2_000, &steps, body(&steps, Duration::from_secs(40), Duration::from_secs(15))).await;
        assert!(!steps.backstop_fired(), "the backstop fired: {failures:?}");
        let live = named(&steps, "live");
        assert_eq!(live.outcome, Outcome::AtDeadline, "{live:?}");
        assert_eq!(live.elapsed, Duration::from_millis(4_900), "{live:?}");
        for name in ["text", "facts", "still_live"] {
            assert_eq!(named(&steps, name).outcome, Outcome::NotReached, "{name}");
        }
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0].starts_with("depth 2000: stopped at the depth deadline in step live after 4.9 s (depth 45.0 s); steps: seed 0.1 s, tab 40.0 s, live 4.9 s stopped at the depth deadline (polls "),
            "{failures:?}"
        );
        assert!(failures[0].ends_with("text not reached, facts not reached, still_live not reached"), "{failures:?}");
    }

    /// A step stopped at the depth deadline mid-evaluate keeps that
    /// evaluate: a single blocked evaluate (M3, M5) must not read like
    /// many small ones (M4). Round 1, finding 1.
    #[tokio::test(start_paused = true)]
    #[ignore]
    async fn test_a_step_stopped_at_the_deadline_keeps_its_evaluate_in_flight() {
        let steps = DepthSteps::new(NAMES, LIMIT);
        let failures = run_depth(2_000, &steps, async {
            let _ = async {
                steps.step("seed", takes(Duration::from_millis(100))).await?;
                steps.step("tab", takes(Duration::from_secs(40))).await?;
                let slow = || async {
                    sleep(Duration::from_secs(30)).await;
                    Ok(true)
                };
                steps.step("live", steps.poll(Duration::from_secs(15), Duration::from_millis(100), OnError::Stop, slow)).await
            }
            .await;
            steps.finish();
            Vec::new()
        })
        .await;
        assert!(!steps.backstop_fired(), "the backstop fired: {failures:?}");
        assert!(
            failures[0].contains("live 4.9 s stopped at the depth deadline (polls 0, max_eval 0.0 s, eval in flight 4.9 s at the deadline)"),
            "{failures:?}"
        );
    }

    /// A failed evaluate's text stays in the report, not only the count:
    /// M5 is told apart by it (`Request timed out` against `context
    /// destroyed`). Round 1, finding 4.
    #[tokio::test(start_paused = true)]
    #[ignore]
    async fn test_a_failed_evaluate_keeps_its_error_text() {
        let steps = DepthSteps::new(NAMES, LIMIT);
        let reads = Cell::new(0);
        let flaky = || {
            reads.set(reads.get() + 1);
            let n = reads.get();
            async move { if n == 1 { Err("Request timed out".to_string()) } else { Ok(true) } }
        };
        steps.step("text", steps.poll(Duration::from_secs(20), Duration::from_millis(200), OnError::Retry, flaky)).await;
        let report = steps.report();
        assert!(report.contains("text 0.2 s (polls 2, max_eval 0.0 s, eval_errors 1, last_error Request timed out)"), "{report}");
    }

    /// The live poll gives up on its first failed evaluate, as
    /// `wait_for_live_client` did (it panicked): polling again would
    /// lengthen the step on that path (SME-329). Round 1, finding 2.
    #[tokio::test(start_paused = true)]
    #[ignore]
    async fn test_a_stop_poll_ends_at_its_first_failed_evaluate() {
        let steps = DepthSteps::new(NAMES, LIMIT);
        let reads = Cell::new(0);
        let flaky = || {
            reads.set(reads.get() + 1);
            let n = reads.get();
            async move { if n == 1 { Err("context destroyed".to_string()) } else { Ok(true) } }
        };
        let live = steps.step("live", steps.poll(Duration::from_secs(15), Duration::from_millis(100), OnError::Stop, flaky)).await;
        assert_eq!(live, Some(false));
        assert_eq!(reads.get(), 1, "{}", steps.report());
        assert_eq!(steps.error_in("live").as_deref(), Some("context destroyed"));
        let report = steps.report();
        assert!(report.contains("live 0.0 s error context destroyed (polls 1, max_eval 0.0 s, eval_errors 1, last_error context destroyed)"), "{report}");
    }

    /// A poll that reaches its own bound gives up, and the depth carries on
    /// to its next steps, as it did before the shared deadline.
    #[tokio::test(start_paused = true)]
    #[ignore]
    async fn test_a_step_at_its_own_bound_lets_the_next_steps_run() {
        let steps = DepthSteps::new(NAMES, LIMIT);
        let failures = run_depth(2_000, &steps, body(&steps, Duration::from_millis(900), Duration::from_secs(15))).await;
        let live = named(&steps, "live");
        assert_eq!(live.outcome, Outcome::GaveUp(Duration::from_secs(15)), "{live:?}");
        assert_eq!(live.elapsed, Duration::from_secs(15), "{live:?}");
        assert!(steps.report().contains("live 15.0 s gave up after its own bound (15.0 s) (polls 151, max_eval 0.0 s)"), "{}", steps.report());
        for name in ["text", "facts", "still_live"] {
            assert_eq!(named(&steps, name).outcome, Outcome::Ok, "{name}");
        }
        assert_eq!(failures, vec!["depth 2000: 1: the client never went live".to_string()]);
        assert!(!steps.backstop_fired());
    }

    /// A depth that never reached `facts` prints `-` for its span depth,
    /// as for its unreached times, not `null`. Round 1, finding 5.
    #[test]
    #[ignore]
    fn test_the_timing_line_prints_a_dash_for_what_it_never_reached() {
        let line = timing_line(2_000, Some(Duration::from_millis(1_200)), None, &serde_json::Value::Null, "seed 0.1 s");
        assert_eq!(
            line,
            "browser tier: markdown_deep_code: depth 2000: live in 1200 ms, rendered in - ms, span depth -; steps: seed 0.1 s"
        );
        let line = timing_line(100, None, None, &serde_json::json!(64), "");
        assert!(line.contains("span depth 64;"), "{line}");
    }

    /// The PM's condition (SME-329): no step gets more time than the old
    /// stacked waits under the outer 45 s gave it. A step that would use
    /// its whole own bound, begun `t` into the depth, runs exactly
    /// min(own, 45 s − t).
    #[tokio::test(start_paused = true)]
    #[ignore]
    async fn test_no_step_gets_more_than_today() {
        // Each step's own bound: sqlx's acquire and chromiumoxide's
        // REQUEST_TIMEOUT for the calls, the helpers' bounds for the polls.
        let own = [("seed", 30), ("tab", 30), ("live", 15), ("text", 20), ("facts", 30), ("still_live", 10)];
        for t in [0, 10_000, 30_000, 40_000, 44_900].map(Duration::from_millis) {
            for (name, own) in own.map(|(n, s)| (n, Duration::from_secs(s))) {
                let steps = DepthSteps::new(NAMES, LIMIT);
                sleep(t).await;
                steps.step(name, async { sleep(own).await; ((), Outcome::GaveUp(own)) }).await;
                let ran = named(&steps, name).elapsed;
                assert_eq!(ran, own.min(LIMIT - t), "{name} begun at {t:?}");
                assert!(ran <= own && ran <= LIMIT - t, "{name} begun at {t:?} ran {ran:?}");
            }
        }
        // The polls themselves, with the scenario's bounds, intervals and
        // error modes, and a slow evaluate (2 s, never true): each gets
        // min(own + one evaluate, 45 s − t), as the harness helpers did
        // under the outer 45 s. Round 1, finding 3.
        let eval = Duration::from_secs(2);
        let polls = [
            ("live", 15, 100, OnError::Stop),
            ("text", 20, 200, OnError::Retry),
            ("still_live", 10, 200, OnError::Retry),
        ];
        for t in [0, 10_000, 30_000, 40_000, 44_900].map(Duration::from_millis) {
            for (name, own, every, on_error) in polls {
                let (own, every) = (Duration::from_secs(own), Duration::from_millis(every));
                let steps = DepthSteps::new(NAMES, LIMIT);
                sleep(t).await;
                let slow = || async move {
                    sleep(eval).await;
                    Ok(false)
                };
                steps.step(name, steps.poll(own, every, on_error, slow)).await;
                let ran = named(&steps, name).elapsed;
                assert!(ran <= (own + eval).min(LIMIT - t), "poll {name} begun at {t:?} ran {ran:?}");
                assert!(ran >= own.min(LIMIT - t), "poll {name} begun at {t:?} gave up early, after {ran:?}");
            }
        }
    }
}
