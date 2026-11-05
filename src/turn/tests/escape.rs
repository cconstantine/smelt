//! SME-386 review: the escape guarantee, as a property. A simulated
//! conversation, model and server drive the real `CompactionProgress` and
//! the real room decisions (`turn::room`) through long random sequences,
//! following the steps `run_turn_steps` and `overflow::recover` take
//! around them, with no database. Two invariants:
//!
//! - after any sequence, a user's send whose request fits with a short
//!   reply reaches the server within a few sends;
//! - wakes alone never keep spending on a conversation that doesn't fit.
//!
//! Three wedges were found by hand in this path (review M-A, M-B, and the
//! final review's M-1); this is so the next one is found by the test.

use super::*;

/// A seeded generator (xorshift64*), so a failure names its case.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[self.below(from.len() as u64) as usize]
    }
}

/// Model steps in one turn, before the simulation calls it done.
const MAX_STEPS: usize = 8;
/// A notice a wake carries, or a cut-off notice.
const NOTICE: u64 = 60;
/// The most a summary takes.
const SUMMARY_MAX: u64 = 3_000;
/// What a compaction quotes of the user's unanswered text, at most (4,000
/// characters).
const QUOTE_MAX: u64 = 1_000;
/// The user's retry ("Again."), in tokens.
const RETRY: u64 = 20;
const WINDOWS: [u32; 9] = [8_192, 12_000, 16_384, 20_000, 24_000, 32_768, 40_000, 65_536, 131_072];

/// How the simulated server treats a request too big for its window.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Server {
    /// Refuses input and `max_tokens` over the window, naming both sizes
    /// (Anthropic, a gateway).
    Counts,
    /// Refuses only input over the window, naming it or not (llama.cpp,
    /// Ollama), and stops a reply at the window.
    Lenient { names: bool },
}

struct Refusal {
    window: Option<u32>,
    input: Option<u64>,
}

enum Called {
    Took { input: u64, written: u64, cut: bool },
    Refused(Refusal),
    Failed,
}

struct World {
    rng: Rng,
    progress: CompactionProgress,
    /// The server's real window.
    server_window: u32,
    /// The window the model's row holds: what the user set, or learned.
    believed: u32,
    /// What no compaction shrinks: the system prompt and tools.
    fixed: u64,
    /// What a compaction summarizes.
    history: u64,
    /// The user's words not yet answered, which a compaction quotes.
    unanswered: u64,
    /// The usage row: the last reply's input and output.
    usage: Option<u64>,
    /// The estimate, as a percentage of the real size.
    estimate_percent: u64,
    server: Server,
    output_cap: Option<u32>,
    fail_percent: u64,
    calls: u32,
    summaries: u32,
    took: u32,
    log: Vec<String>,
}

impl World {
    fn new(seed: u64) -> Self {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let server_window = rng.pick(&WINDOWS);
        let fixed = rng.range(1_500, u64::from(server_window) * 3 / 4);
        World {
            server_window,
            believed: if rng.chance(70) { server_window } else { rng.pick(&WINDOWS) },
            fixed,
            history: 0,
            unanswered: 0,
            usage: None,
            estimate_percent: rng.range(85, 115),
            server: if rng.chance(50) { Server::Counts } else { Server::Lenient { names: rng.chance(70) } },
            output_cap: None,
            fail_percent: 5,
            calls: 0,
            summaries: 0,
            took: 0,
            log: Vec::new(),
            progress: CompactionProgress::default(),
            rng,
        }
    }

    fn estimate(&self, tokens: u64) -> u64 {
        tokens * self.estimate_percent / 100
    }

    fn real(&self) -> u64 {
        self.fixed + self.history
    }

    fn compact(&mut self) {
        self.summaries += 1;
        self.history = self.rng.range(300, SUMMARY_MAX) + self.unanswered.min(QUOTE_MAX);
        self.usage = None;
        self.log.push(format!("  compact -> {}", self.real()));
    }

    fn call(&mut self, max_tokens: u32) -> Called {
        self.calls += 1;
        let input = self.real();
        if self.rng.chance(self.fail_percent) {
            self.log.push(format!("  call {input}+{max_tokens}: failed"));
            return Called::Failed;
        }
        let window = u64::from(self.server_window);
        let over = input > window || (self.server == Server::Counts && input + u64::from(max_tokens) > window);
        if over {
            let names = matches!(self.server, Server::Counts | Server::Lenient { names: true });
            self.log.push(format!("  call {input}+{max_tokens}: refused (names: {names})"));
            return Called::Refused(Refusal { window: names.then_some(self.server_window), input: names.then_some(input) });
        }
        let wants = self.rng.range(100, 12_000);
        let written = wants.min(u64::from(max_tokens)).min(window - input);
        self.took += 1;
        self.log.push(format!("  call {input}+{max_tokens}: took, wrote {written} of {wants}"));
        Called::Took { input, written, cut: written < wants }
    }

    /// `build_request_with_room`.
    fn build(&mut self, window: u32, pending: u64, compacted: &mut bool, hold_to_room: bool, refused: bool, undercount: u64) -> Result<ReplyBudget, String> {
        loop {
            let room_only = self.progress.room_only_at(window) || hold_to_room;
            let projected = self.usage.map_or_else(|| self.estimate(self.real()) + undercount, |usage| usage + self.estimate(pending));
            let budget = budget_for(window, projected, self.output_cap, room_only);
            let size = self.estimate(self.real()) + undercount;
            match before_sending(&mut self.progress, "m", window, self.output_cap, room_only, budget, size, *compacted) {
                BeforeSending::Send(sent) => return Ok(sent),
                BeforeSending::Compact => {
                    self.compact();
                    if refused {
                        self.progress.refusal_compacted(window);
                    } else {
                        self.progress.compacted(window);
                    }
                    *compacted = true;
                }
                BeforeSending::Refuse(why) => return Err(why),
            }
        }
    }

    fn learn(&mut self, window: &mut u32, named: Option<u32>) {
        if let Some(named) = named {
            *window = (*window).min(named);
            self.believed = self.believed.min(named);
        }
    }

    /// `overflow::recover`: the reply, its budget, and whether it compacted.
    fn recover(&mut self, window: &mut u32, pending: u64, refusal: &Refusal) -> Result<(u64, u64, bool, ReplyBudget, bool), String> {
        self.learn(window, refusal.window);
        let input = refusal.input.unwrap_or_else(|| self.estimate(self.real()));
        let believed = *window;
        let alone = self.estimate(self.fixed);
        let undercount = refusal.input.map_or(0, |input| input.saturating_sub(self.estimate(self.real())));
        let (mut compacted, hold_to_room) = match refusal.window {
            Some(_) => match after_named_refusal("m", *window, alone, input) {
                NamedRefusal::TooSmall(why) => {
                    self.progress.too_small(alone, *window);
                    return Err(why);
                }
                NamedRefusal::Retry { compact } => (compact, true),
            },
            None => {
                if let Some(assumed) = guessed_window(input, believed) {
                    *window = assumed;
                    self.progress.guessed(believed, assumed);
                }
                (true, false)
            }
        };
        if compacted {
            self.compact();
            self.progress.refusal_compacted(*window);
        } else {
            // The retry sizes its budget from what the server counted too.
            let projected = self.usage.map_or(0, |usage| usage + self.estimate(pending));
            self.usage = Some(projected.max(input));
        }
        let budget = self.build(*window, 0, &mut compacted, hold_to_room, true, undercount)?;
        match self.call(budget.tokens) {
            Called::Took { input, written, cut } => {
                self.progress.retry_taken();
                Ok((input, written, cut, budget, compacted))
            }
            Called::Refused(again) => {
                self.progress.refused(again.input);
                self.learn(window, again.window);
                Err("still didn't fit".to_string())
            }
            Called::Failed => Err("failed".to_string()),
        }
    }

    /// `run_turn_steps`, for a user's send of `typed` tokens or a wake.
    fn turn(&mut self, typed: Option<u64>) -> Result<(), String> {
        let mut window = self.believed;
        let mut pending = match typed {
            Some(typed) => {
                self.progress.user_wrote();
                self.unanswered += typed;
                typed
            }
            None => NOTICE,
        };
        self.history += pending;
        for _ in 0..MAX_STEPS {
            let projected = self.usage.map(|usage| usage + self.estimate(pending));
            let mut compacted = false;
            match self.progress.step("m", window, projected) {
                CompactionStep::Stuck(why) => return Err(why),
                CompactionStep::Compact => {
                    self.compact();
                    self.progress.compacted(window);
                    compacted = true;
                }
                CompactionStep::Go => {}
            }
            let budget = self.build(window, pending, &mut compacted, false, false, 0)?;
            let mut sized = self.usage.map(|usage| usage + self.estimate(pending));
            let (input, written, cut, budget) = match self.call(budget.tokens) {
                Called::Took { input, written, cut } => {
                    self.progress.step_taken();
                    (input, written, cut, budget)
                }
                Called::Refused(refusal) => {
                    if let Some(why) = self.progress.why_refused_out("m", window) {
                        return Err(why);
                    }
                    self.progress.refused(refusal.input);
                    let (input, written, cut, budget, compacted) = self.recover(&mut window, pending, &refusal)?;
                    if compacted {
                        sized = None;
                    }
                    (input, written, cut, budget)
                }
                Called::Failed => return Err("failed".to_string()),
            };
            let usage = anthropic::TokenUsage {
                input_tokens: input as i64,
                output_tokens: written as i64,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            };
            self.progress.measured(&usage, window);
            self.usage = Some(input + written);
            self.history += written;
            self.unanswered = 0;
            if !cut {
                self.progress.reply_finished();
            }
            if cut && budget.limit == crate::api::chat::ReplyLimit::RoomLeft && self.progress.cut_for_room(sized, window) {
                pending = NOTICE;
                self.history += NOTICE;
                continue;
            }
            if cut {
                return self.progress.why_if_stuck("m", window).map_or(Ok(()), Err);
            }
            if self.rng.chance(50) {
                pending = self.rng.range(50, 9_000);
                self.history += pending;
                continue;
            }
            return Ok(());
        }
        Ok(())
    }

    fn send(&mut self, typed: u64) -> Result<(), String> {
        self.log.push(format!("send {typed}: fixed {}, window {} (believed {}), {:?}", self.fixed, self.server_window, self.believed, self.server));
        let result = self.turn(Some(typed));
        self.log.push(format!("  -> {result:?}"));
        result
    }

    fn wake(&mut self) -> Result<(), String> {
        self.log.push(format!("wake: fixed {}, window {} (believed {})", self.fixed, self.server_window, self.believed));
        let result = self.turn(None);
        self.log.push(format!("  -> {result:?}"));
        result
    }

    /// One random thing happening to the conversation.
    fn event(&mut self) {
        match self.rng.below(100) {
            0..=34 => {
                let typed = if self.rng.chance(70) { self.rng.range(5, 500) } else { self.rng.range(1_000, 30_000) };
                let _ = self.send(typed);
            }
            35..=59 => {
                let _ = self.wake();
            }
            60..=69 => {
                // An MCP server added or removed, a longer system prompt.
                let change = self.rng.range(0, 12_000) as i64 - 4_000;
                self.fixed = (self.fixed as i64 + change).clamp(1_500, 120_000) as u64;
                self.log.push(format!("fixed -> {}", self.fixed));
            }
            70..=74 => {
                self.believed = if self.rng.chance(50) { self.server_window } else { self.rng.pick(&WINDOWS) };
                self.log.push(format!("believed -> {}", self.believed));
            }
            75..=77 => {
                self.server_window = self.rng.pick(&WINDOWS);
                self.log.push(format!("server window -> {}", self.server_window));
            }
            78..=84 => self.estimate_percent = self.rng.range(85, 115),
            85..=89 => self.output_cap = self.rng.chance(50).then(|| self.rng.range(512, 32_000) as u32),
            90..=94 => self.server = if self.rng.chance(50) { Server::Counts } else { Server::Lenient { names: self.rng.chance(70) } },
            _ => self.fail_percent = if self.rng.chance(50) { 0 } else { 20 },
        }
    }

    /// Whether a compacted request, by the server's count and by smelt's
    /// estimate both, fits with a short reply beside the user's retry, in
    /// the server's window and the one the user set alike.
    fn fits_a_short_reply(&self) -> bool {
        let compacted = self.fixed + SUMMARY_MAX + QUOTE_MAX + RETRY;
        let window = self.server_window.min(self.believed);
        compacted.max(self.estimate(compacted)) + u64::from(MIN_USEFUL_REPLY) <= u64::from(window)
    }
}

/// How many cases, each a sequence of up to `EVENTS` events, then the checks.
const CASES: u64 = 20_000;
const EVENTS: u64 = 40;
/// User sends within which one that fits must reach the server.
const SENDS: usize = 3;
/// Wakes in a row that may fail after spending with no step taken, with
/// nothing else changing: three compactions answering refusals, and one more
/// refused call.
const SPENDING_FAILURES: usize = 4;

fn run_case(seed: u64) -> (Result<(), String>, Vec<String>) {
    let mut world = World::new(seed);
    let result = check(&mut world);
    (result, world.log)
}

fn check(world: &mut World) -> Result<(), String> {
    let events = world.rng.below(EVENTS + 1);
    for _ in 0..events {
        world.event();
    }
    world.fail_percent = 0;

    // Wakes alone stop spending on a conversation that doesn't fit.
    let mut spending_failures = 0;
    for _ in 0..10 {
        let (spent, took) = ((world.calls, world.summaries), world.took);
        match world.wake() {
            // A wake the server took a step of got somewhere.
            Err(_) if world.took == took && (world.calls, world.summaries) != spent => spending_failures += 1,
            Err(_) if world.took == took => {}
            _ => spending_failures = 0,
        }
        if spending_failures > SPENDING_FAILURES {
            return Err(format!("{spending_failures} wakes in a row failed after spending"));
        }
    }

    // A send that fits reaches the server.
    if world.fits_a_short_reply() {
        let took = world.took;
        for _ in 0..SENDS {
            let _ = world.send(RETRY);
        }
        if world.took == took {
            return Err(format!("{SENDS} sends that fit never reached the server"));
        }
    }
    Ok(())
}

#[test]
fn test_a_send_that_fits_always_gets_through_and_wakes_stop_spending() {
    let mut failures = Vec::new();
    for seed in 0..CASES {
        if let (Err(why), log) = run_case(seed) {
            failures.push((seed, why, log));
        }
    }
    // `SME386_ESCAPE_DUMP=<file>` writes every failing case's log there.
    if let Ok(path) = std::env::var("SME386_ESCAPE_DUMP") {
        let dump: Vec<String> = failures.iter().map(|(seed, why, log)| format!("== seed {seed}: {why}\n{}", log.join("\n"))).collect();
        std::fs::write(path, dump.join("\n")).expect("write the dump");
    }
    if let Some((seed, why, log)) = failures.first() {
        let tail = log.len().saturating_sub(60);
        let log = &log[tail..];
        panic!(
            "{} of {CASES} cases failed; the first, seed {seed}: {why}\nthe end of its log:\n{}",
            failures.len(),
            log.join("\n")
        );
    }
}
