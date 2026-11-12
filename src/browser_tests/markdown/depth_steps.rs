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
    Error(String),
}

/// A polling step's counts: how many evaluates, the slowest one, how many
/// failed, and when the one still in flight started (from the depth's
/// start).
#[derive(Clone, Debug, Default)]
pub(super) struct PollStats {
    pub(super) polls: u32,
    pub(super) max_eval: Duration,
    pub(super) eval_errors: u32,
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

    /// Runs `fut` as step `name`, which gives back its value and how it
    /// ended.
    pub(super) async fn step<T>(&self, name: &'static str, fut: impl Future<Output = (T, Outcome)>) -> T {
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
        let (value, outcome) = fut.await;
        let mut steps = self.steps.borrow_mut();
        let step = &mut steps[index];
        step.elapsed = begun.elapsed();
        step.outcome = outcome;
        value
    }

    /// Polls `eval` until it reads true, every `interval`, giving up once
    /// `own` has passed (checked between evaluates, as the harness's
    /// `wait_for_live_client` and `wait_for_text` do). Counts each evaluate
    /// into the current step, so a step that stalls mid-poll still shows
    /// them, and the one in flight.
    pub(super) async fn poll<F, Fut>(&self, own: Duration, interval: Duration, mut eval: F) -> (bool, Outcome)
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
                if read.is_err() {
                    s.eval_errors += 1;
                }
            });
            if read == Ok(true) {
                return (true, Outcome::Ok);
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
        Outcome::Error(e) => format!("{} {} error {e}", s.name, secs(s.elapsed)),
    };
    if let Some(stats) = &s.stats {
        line += &format!(" (polls {}, max_eval {}", stats.polls, secs(stats.max_eval));
        if stats.eval_errors > 0 {
            line += &format!(", eval_errors {}", stats.eval_errors);
        }
        if let Some(since) = stats.in_flight {
            line += &format!(", eval in flight {}", secs(now.saturating_sub(since)));
        }
        line += ")";
    }
    line
}

/// Runs one depth's `body` under the deadline `steps` was made with. A
/// depth that hangs or panics ends alone, so the rest still run; its
/// failures come back prefixed with the depth, and a hang or a panic names
/// its step.
pub(super) async fn run_depth(depth: usize, steps: &DepthSteps, body: impl Future<Output = Vec<String>>) -> Vec<String> {
    let outcome =
        futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(tokio::time::timeout_at(steps.deadline(), body)))
            .await;
    let problems = match outcome {
        Ok(Ok(problems)) => problems,
        Ok(Err(_)) => vec![steps.timeout_report()],
        Err(panic) => vec![format!(
            "{}; steps: {}",
            panic.downcast_ref::<String>().cloned().unwrap_or_else(|| "panicked".to_string()),
            steps.report()
        )],
    };
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
}
