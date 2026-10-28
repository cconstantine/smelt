//! Id allocation for the bands whose ids name cluster objects (SME-255).
//!
//! `db::clear_ids_base()` used to fold wall-clock nanoseconds through a
//! one-second modulo: two near-simultaneous callers shared a clock tick
//! and got the same base, and the pods and claims derived from it met by
//! name in the one shared test namespace — the `NotOurs`/409 flake family
//! (docs/testing.md, "Known infrastructure failures"). Clock granularity,
//! not luck, decides whether two folds meet, so no id is folded from the
//! clock here: each band has one cursor, and every call advances it.
//!
//! * **one process**: each call returns the cursor plus [`ID_STRIDE`], so
//!   no two callers share a value and consecutive values sit far apart —
//!   a lifecycle scenario uses a few dozen rows in the id-naming tables,
//!   a thousandfold under the stride.
//! * **across processes**: the cursor is seeded once per process from
//!   nanoseconds mixed with the process id, and the cluster lock
//!   (`scripts/with-cluster-lock`, enforced by `src/test_lock.rs`,
//!   SME-212) keeps that to one test binary at a time — re-runs meet only
//!   if their seeds land within a few strides of each other, and the
//!   harness's sweep (SME-134 with SME-255's arm) takes a finished run's
//!   leftovers whatever their names.
//! * **past the reserve**: each band holds back a documented reserve of
//!   calls (GLOBAL_IDS 4,000, FIXTURE_IDS 8,000 — each at least four
//!   times the server suite's heaviest draw, ≈1,000 and ≈2,100) from its
//!   seed window, so even the worst seed leaves a full reserve above it.
//!   A process that asks for more than the reserve is a pathological
//!   loop: it panics by name rather than wrapping into spent values. A
//!   caller that panics after its base leaves it spent — it is never
//!   re-handed.
//!
//! Nothing survives the process by design, and the seed is read once, so
//! a clock that steps backward (NTP) cannot matter. Tests assert the
//! invariants (band membership, distinctness), never a value.
//! `src/repo_lints/id_alloc.rs` refuses to let a new clock-fold id site
//! join this module. (`src/bin/sandbox_instance_cleanup/tests.rs`'s
//! `unique()` is the same scheme restated locally — that bin is its own
//! crate and cannot reach here.)

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::OnceLock;

/// The gap between two allocations from one band: far above any test's
/// row count in the id-naming tables, and far below a band's width.
pub(crate) const ID_STRIDE: i64 = 100_000;

/// One id band: the half-open range `[base, base + width)`, and this
/// process's cursor through it.
pub(crate) struct Band {
    /// The band's name, for the exhaustion panic.
    name: &'static str,
    base: i64,
    width: i64,
    /// Calls this band guarantees a process, whatever seed it draws: the
    /// seed window gives up `reserve × ID_STRIDE` from the band's top, so
    /// the worst seed still leaves the whole reserve above it, and the
    /// assert in `next_base` guards only loops past that reserve.
    reserve: i64,
    /// The seed for this process: the clock mixed with the pid, read on
    /// the first call and never again.
    seed: OnceLock<i64>,
    /// Steps taken, counting by `ID_STRIDE` from the seed; `fetch_add`
    /// hands each step to exactly one caller.
    steps: AtomicI64,
}

impl Band {
    const fn new(name: &'static str, base: i64, width: i64, reserve: i64) -> Self {
        Band { name, base, width, reserve, seed: OnceLock::new(), steps: AtomicI64::new(0) }
    }

    /// The largest seed the draw can hand out: the top of the window
    /// that remains once the reserve is held back.
    #[cfg(test)]
    fn worst_seed(&self) -> i64 {
        self.base + (self.width - self.reserve * ID_STRIDE) - 1
    }

    /// The next base in this band: `ID_STRIDE` above the last one this
    /// process took, always inside `[base, base + width)`.
    pub(crate) fn next_base(&'static self) -> i64 {
        let seed = *self.seed.get_or_init(|| {
            use std::time::{SystemTime, UNIX_EPOCH};
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default() as u64;
            let pid = u64::from(std::process::id());
            // A splitmix64 finalizer over the two: neighbouring pids and
            // neighbouring clock ticks both land far apart in the band.
            let mut x = nanos ^ pid.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            x ^= x >> 30;
            x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x ^= x >> 27;
            x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
            x ^= x >> 31;
            // Keep the whole reserve of advances inside the band: the
            // window gives up the reserve from its top, so even the
            // worst seed leaves `reserve` strides below the band end.
            self.base + (x % (self.width - self.reserve * ID_STRIDE) as u64) as i64
        });
        let value = seed + (self.steps.fetch_add(ID_STRIDE, Ordering::Relaxed) + ID_STRIDE);
        assert!(
            value < self.base + self.width,
            "the {} id band ({} wide from {}) is exhausted: {:?} would leave it",
            self.name,
            self.width,
            self.base,
            value
        );
        value
    }
}

/// The band for the sequences that name pods and claims:
/// `[2_000_000_000, 3_000_000_000)`. `db::clear_ids_base()` — the harness
/// moves and the production fresh-database start alike — is its
/// `next_base`. Observed worst single process: ≈1,000 calls (T1's 512 +
/// T2's 200 + one gate run's harness moves) — the reserve of 4,000
/// leaves four times that as seed-window room.
pub(crate) static GLOBAL_IDS: Band = Band::new("GLOBAL_IDS", 2_000_000_000, 1_000_000_000, 4_000);

/// The band for the ids a test picks by hand: fixture conversation and
/// pod ids, the session-name suffixes that become ephemeral pod names,
/// and the offsets that move a test database's own sequences away:
/// `[3_000_000_000, 4_000_000_000)`. The old hand-picked `[1e9, 2e9)`
/// band retired with SME-255: two tests that want disjoint ids call the
/// allocator twice, and a fold's shared tick can no longer hand two of
/// them the same value. Observed worst single process: ≈2,100 calls
/// (T5 alone draws 2,000) — the reserve of 8,000 leaves the rest of the
/// band to the seed window.
#[cfg(test)]
pub(crate) static FIXTURE_IDS: Band = Band::new("FIXTURE_IDS", 3_000_000_000, 1_000_000_000, 8_000);

#[cfg(test)]
mod tests {
    use super::*;

    /// Reviewer A-F1: the seed is drawn from the band's whole width, so
    /// the calls left at the worst seed are `(band end − worst seed) /
    /// stride` — and that must cover the heaviest run, or the exhaustion
    /// panic fires on legitimate traffic. The reserve each band documents
    /// is the promise; the seed window and the reserve are one constant,
    /// so the pin that breaks when the two drift apart is this relation:
    /// **the worst seed plus the whole reserve stays under the band end.**
    /// Red against the seed-draws-what-it-likes form: worst seed + 8,000
    /// strides lands 800 million past the FIXTURE band's end.
    #[test]
    fn test_the_worst_seed_leaves_the_whole_reserve() {
        let demand = [("GLOBAL_IDS", &GLOBAL_IDS, 4_000_i64), ("FIXTURE_IDS", &FIXTURE_IDS, 8_000)];
        for (name, band, calls) in demand {
            assert!(band.reserve * ID_STRIDE < band.width, "{name}: the reserve must leave a seed window at all");
            assert!(
                band.reserve >= calls,
                "{name}: the run's documented worst case is {calls} calls (see the band's comment), the reserve is {}",
                band.reserve
            );
            assert!(
                band.worst_seed() + band.reserve * ID_STRIDE < band.base + band.width,
                "{name}: the worst seed leaves room for only {}, not the reserve of {}",
                (band.base + band.width - band.worst_seed()) / ID_STRIDE,
                band.reserve
            );
        }
    }
}
