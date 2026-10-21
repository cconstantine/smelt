//! The tests' half of the cluster lock (SME-212).
//!
//! Every `#[sqlx::test]` and every real-cluster test shares the test
//! Postgres server and the k3s cluster's `smelt-park-test` namespace with
//! every other test run on this machine, so it runs only under the cluster
//! lock (docs/testing.md). `scripts/with-cluster-lock` tells the command it
//! runs that the lock is held, through `SMELT_CLUSTER_LOCK_HELD`; these
//! tests check for it before touching either, and refuse at once with how
//! to run them instead. A bare or substring-filtered `cargo test` then fails
//! those tests by name rather than dropping another run's databases
//! (SME-172, SME-111, SME-115). A pure unit test doesn't call this, so it
//! runs anywhere.
//!
//! Compiled only into test builds: the smelt binary's tests and the two
//! helper binaries' (`#[path]`), which have real-cluster tests of their own.

use sqlx::migrate::Migrator;

/// The variable `scripts/with-cluster-lock` sets to the lock file's path
/// for the command it runs, once the lock is held.
const HELD: &str = "SMELT_CLUSTER_LOCK_HELD";

/// Why the test can't run, or `Ok` when `with-cluster-lock` said the lock
/// is held: `held` is `SMELT_CLUSTER_LOCK_HELD`'s value.
pub(crate) fn check(held: Option<&str>) -> Result<(), String> {
    match held {
        Some(path) if !path.is_empty() => Ok(()),
        _ => Err(format!(
            "this test uses the shared test Postgres or the k3s cluster, so it runs only under the \
             cluster lock: run it with scripts/cluster-test <cargo test args> (one test: -- --exact \
             <full path>) or scripts/check.sh, which take the lock and set {HELD} for the tests \
             (docs/testing.md, SME-212)."
        )),
    }
}

/// Panics with [`check`]'s message unless `SMELT_CLUSTER_LOCK_HELD` is set.
/// A panic per test, not an exit: libtest names each refused test, and the
/// pure tests in the same run still report. Read with `std::env`, never
/// dotenvy, so a `.env` line can't stand in for it before the first sqlx
/// call loads `.env`.
pub(crate) fn require() {
    let held = std::env::var(HELD).ok();
    if let Err(why) = check(held.as_deref()) {
        panic!("{why}");
    }
}

/// A migrator that refuses the test before sqlx connects, unless the lock
/// is held. Every `#[sqlx::test]` names one of the two statics below
/// (`migrator = "crate::test_lock::MIGRATOR"`): sqlx's macro expands that to
/// `args.migrator(&<path>)` ahead of `run_test`, which is where it creates
/// the test database (sqlx-macros-core 0.8.6, `test_attr.rs`), and the
/// reference coerces to `&Migrator` through this `Deref`. A `Deref` with a
/// side effect, on purpose: it's the one hook sqlx runs first.
/// `src/repo_lints/cluster_lock.rs` fails the gate on an attribute that
/// names neither.
pub(crate) struct Locked(&'static Migrator);

impl std::ops::Deref for Locked {
    type Target = Migrator;

    fn deref(&self) -> &Migrator {
        require();
        self.0
    }
}

static MIGRATIONS: Migrator = sqlx::migrate!();
/// No migrations: `Migrator::DEFAULT` is the empty list sqlx's own
/// `migrate!()` builds on. For a test that migrates by hand.
static NONE: Migrator = Migrator::DEFAULT;

/// The repo's migrations, applied once the lock is known to be held.
pub(crate) static MIGRATOR: Locked = Locked(&MIGRATIONS);
/// No migrations (the old `migrations = false`), under the same check.
pub(crate) static NO_MIGRATIONS: Locked = Locked(&NONE);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_marker_refuses_and_says_how_to_run_it() {
        for held in [None, Some("")] {
            let why = check(held).expect_err("refused without the marker");
            assert!(why.contains("scripts/cluster-test"), "{held:?}: {why}");
            assert!(why.contains("scripts/check.sh"), "{held:?}: {why}");
            // Review 1: the scripts set the marker; the message doesn't
            // point at it as the thing to set.
            assert!(why.contains("which take the lock and set SMELT_CLUSTER_LOCK_HELD"), "{held:?}: {why}");
            assert!(!why.contains("is unset"), "{held:?}: {why}");
        }
    }

    #[test]
    fn test_the_marker_lets_it_run() {
        assert_eq!(check(Some("/x/.git/smelt-cluster.lock")), Ok(()));
    }
}
