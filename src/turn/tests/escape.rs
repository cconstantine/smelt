//! SME-386 review: the escape guarantee, as a property. A simulated
//! conversation, model and server drive the real `CompactionProgress` and
//! the real room decisions (`turn::room`) through long random sequences,
//! following the steps `run_turn_steps` and `overflow::recover` take
//! around them, with no database. Three invariants:
//!
//! - after any sequence, a user's send whose request fits with a short
//!   reply reaches the server within a few sends;
//! - wakes alone never keep spending on a conversation that doesn't fit;
//! - the floor never refuses a compacted retry answering a count-naming
//!   refusal when that retry fits — by the server's truth as well as the
//!   estimate, with the undercount share the fix keeps (SME-421) and the
//!   buffer and a short reply added on top.
//!
//! Three wedges were found by hand in this path (review M-A, M-B, and the
//! final review's M-1); this is so the next one is found by the test. The
//! world models a per-part estimate error (SME-421): a base64/CJK-style
//! blob in the history with its own, much lower, estimate ratio — the
//! part a chars/4 estimate misses by 2–5× and a summary then replaces.

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
    /// Refuses input and `max_tokens` over the window. `names_input` says
    /// whether the refusal names the input too: a gateway names both
    /// (Anthropic); a vLLM counts `max_tokens` and names only the window
    /// (SME-422's wedge server).
    Counts { names_input: bool },
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
    /// Real tokens of a base64/CJK-style tool output sitting in the
    /// history (SME-421): the part `estimate_percent`'s ratio misses,
    /// estimated at `blob_percent` instead, and zeroed by `compact()`.
    blob: u64,
    /// The user's words not yet answered, which a compaction quotes.
    unanswered: u64,
    /// The usage row: the last reply's input and output.
    usage: Option<u64>,
    /// The estimate, as a percentage of the real size, of everything but
    /// the blob: prose the chars/4 estimate tracks.
    estimate_percent: u64,
    /// The blob's estimate, as a percentage of its real size: a base64 or
    /// CJK run tokenizes at 2–5× what chars/4 guesses (SME-421).
    blob_percent: u64,
    server: Server,
    output_cap: Option<u32>,
    fail_percent: u64,
    calls: u32,
    summaries: u32,
    took: u32,
    /// What the compacted retry built to answer the current count-naming
    /// refusal will be charged for the shortfall: the third invariant's
    /// bound (SME-421), set across one `recover` and cleared with it.
    expect_send: Option<u64>,
    /// A broken invariant the run does not end at: the third one's fault,
    /// which `check` reports first (SME-421).
    faults: Vec<String>,
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
            blob: 0,
            unanswered: 0,
            usage: None,
            estimate_percent: rng.range(85, 115),
            blob_percent: rng.range(20, 60),
            server: if rng.chance(50) { Server::Counts { names_input: rng.chance(50) } } else { Server::Lenient { names: rng.chance(70) } },
            output_cap: None,
            fail_percent: 5,
            calls: 0,
            summaries: 0,
            took: 0,
            expect_send: None,
            faults: Vec::new(),
            log: Vec::new(),
            progress: CompactionProgress::default(),
            rng,
        }
    }

    fn estimate(&self, tokens: u64) -> u64 {
        tokens * self.estimate_percent / 100
    }

    /// The whole request's estimate as `estimate_request_tokens` makes it:
    /// each history part at its own ratio (SME-421). New content — typed,
    /// pending, a compacted hypothetical — has no blob in it, and stays
    /// proportional at `estimate`.
    fn estimate_request(&self) -> u64 {
        self.estimate(self.fixed + self.history) + self.blob * self.blob_percent / 100
    }

    fn real(&self) -> u64 {
        self.fixed + self.history + self.blob
    }

    fn compact(&mut self) {
        self.summaries += 1;
        self.history = self.rng.range(300, SUMMARY_MAX) + self.unanswered.min(QUOTE_MAX);
        // A summary keeps a base64 blob's gist in plain words, not its
        // bytes: the under-counted part's estimate share collapses here
        // while a refusal's count of the refused request still carries it.
        self.blob = 0;
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
        let counts_budget = matches!(self.server, Server::Counts { .. });
        let over = input > window || (counts_budget && input + u64::from(max_tokens) > window);
        if over {
            // The window is named by any naming shape; the input only by
            // a server that counted it in its message (SME-422).
            let names_window = matches!(self.server, Server::Counts { .. } | Server::Lenient { names: true });
            let names_input = match self.server {
                Server::Counts { names_input } => names_input,
                Server::Lenient { names } => names,
            };
            self.log.push(format!("  call {input}+{max_tokens}: refused (window: {names_window}, input: {names_input})"));
            return Called::Refused(Refusal { window: names_window.then_some(self.server_window), input: names_input.then_some(input) });
        }
        let wants = self.rng.range(100, 12_000);
        let written = wants.min(u64::from(max_tokens)).min(window - input);
        self.took += 1;
        self.log.push(format!("  call {input}+{max_tokens}: took, wrote {written} of {wants}"));
        Called::Took { input, written, cut: written < wants }
    }

    /// `build_request_with_room`.
    fn build(&mut self, window: u32, pending: u64, compacted: &mut bool, hold_to_room: bool, refused: bool, undercount: Undercount) -> Result<ReplyBudget, String> {
        loop {
            let room_only = self.progress.room_only_at(window) || hold_to_room;
            let correction = undercount.for_request(*compacted);
            let projected = self.usage.map_or_else(|| self.estimate_request() + correction, |usage| usage + self.estimate(pending));
            let budget = budget_for(window, projected, self.output_cap, room_only);
            let size = self.estimate_request() + correction;
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
                BeforeSending::Refuse(why) => {
                    // SME-421's invariant: this request answers a refusal
                    // that named its count, and it fits — the server's own
                    // count and the estimate both, with the surviving
                    // share the correction adds, the estimate's buffer and
                    // a short reply to spare. A mid-loop compaction only
                    // shrank the sizes below, so this holds after one too.
                    if let Some(bound) = self.expect_send
                        && self
                            .real()
                            .max(self.estimate_request())
                            .saturating_add(bound)
                            .saturating_add(u64::from(COMPACTION_SAFETY_BUFFER) + u64::from(MIN_USEFUL_REPLY))
                            <= u64::from(window)
                    {
                        let fault = format!(
                            "the floor refused a compacted retry that fits: real {}, est {}, bound {bound}, window {window}",
                            self.real(),
                            self.estimate_request(),
                        );
                        self.log.push(format!("  FAULT: {fault}"));
                        self.faults.push(fault);
                    }
                    return Err(why);
                }
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
        let input = refusal.input.unwrap_or_else(|| self.estimate_request());
        let believed = *window;
        let alone = self.estimate(self.fixed);
        // The estimate of the request this refusal refused, before any
        // compaction: read before `compact()` zeroes the blob, and the
        // same number `undercount` is built from.
        let refused_estimate = self.estimate_request();
        let undercount = Undercount::from_refusal(refusal.input, refused_estimate, alone);
        let (mut compacted, hold_to_room, short_reply) = match refusal.window {
            Some(_) => match after_named_refusal("m", *window, alone, refusal.input) {
                NamedRefusal::TooSmall(why) => {
                    self.progress.too_small(alone, *window);
                    return Err(why);
                }
                NamedRefusal::Retry { compact, short_reply } => (compact, true, short_reply),
            },
            None => {
                if let Some(assumed) = guessed_window(input, believed) {
                    *window = assumed;
                    self.progress.guessed(believed, assumed);
                }
                (true, false, false)
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
        // This retry answers a refusal that named its count. The oracle's
        // bound — what the corrected charge for a compacted retry may be,
        // per the fix — is computed from the server's truth and `alone`,
        // never from the code under test: `full`'s share of the surviving
        // (fixed, unshrunk) part. A retry that fits even with it on must
        // not be refused (SME-421); a mid-loop compaction keeps the bound,
        // as it only shrinks the sizes below.
        self.expect_send = refusal.input.map(|input| {
            let full = input.saturating_sub(refused_estimate);
            if refused_estimate == 0 { full } else { full.saturating_mul(alone) / refused_estimate }
        });
        let budget = self.build(*window, 0, &mut compacted, hold_to_room, true, undercount);
        self.expect_send = None;
        let budget = budget?;
        // A refusal that named no input vouches for none of the room this
        // budget takes: the retry answering it takes only the short reply,
        // exactly as `overflow::recover` does (SME-422).
        let budget = if short_reply { short_reply_budget(budget) } else { budget };
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
                // A big tool output lands with the ask some third of the
                // time (SME-421's blob): bytes the estimate misses.
                if self.rng.chance(30) {
                    self.blob += self.rng.range(200, 30_000);
                }
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
            let budget = self.build(window, pending, &mut compacted, false, false, Undercount::none())?;
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
                if self.rng.chance(30) {
                    self.blob += self.rng.range(200, 30_000);
                }
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
            78..=84 => {
                self.estimate_percent = self.rng.range(85, 115);
                self.blob_percent = self.rng.range(20, 60);
            }
            85..=89 => self.output_cap = self.rng.chance(50).then(|| self.rng.range(512, 32_000) as u32),
            90..=94 => self.server = if self.rng.chance(50) { Server::Counts { names_input: self.rng.chance(50) } } else { Server::Lenient { names: self.rng.chance(70) } },
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
    // The third invariant first: its fault is what this widening hunts.
    if let Some(first) = world.faults.first() {
        return Err(first.clone());
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
    if let Some(first) = world.faults.first() {
        return Err(first.clone());
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
