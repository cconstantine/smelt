//! Deletes the pods and claims in `smelt-park-test` that are more than an
//! hour old, once per test process (SME-134). A killed run, an aborted
//! test binary or a failed teardown leaves its objects there, and no later
//! run reuses their names, so nothing else ever deletes them.
//!
//! Age is what makes an object safe to delete: no test keeps one alive for
//! anywhere near an hour, so an object that old belongs to a run that has
//! ended or hung. Names, ids and instance labels don't count: every object
//! in the namespace is a test's.
//!
//! A second bound catches a killed run's leftovers inside the hour: under
//! the cluster lock no other run is live when this process allocates, so
//! any object created before **this process started** belongs to one that
//! has ended (SME-255). Objects created after the start — this run's — are
//! never taken by it, and a Terminating object is never taken by either
//! bound.
//!
//! It never reaches `smelt-park`, the user's live namespace, by three
//! separate guards:
//! 1. this module is test-only (a child of `pod_lifecycle_tests`), so no
//!    binary contains it;
//! 2. its `Api`s are built only from the literal `SWEEP_NAMESPACE`, and it
//!    refuses to run unless that is the tests' namespace and not
//!    `smelt-park`;
//! 3. `choose` drops any object whose own namespace isn't
//!    `SWEEP_NAMESPACE`.

use std::time::Duration;

use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use k8s_openapi::jiff::{SignedDuration, Timestamp};
use futures_util::stream::{self, StreamExt};
use kube::api::{Api, DeleteParams, ListParams, ObjectMeta, PostParams, Preconditions};

/// The only namespace the sweep lists or deletes in.
const SWEEP_NAMESPACE: &str = "smelt-park-test";

/// The user's live namespace, which the sweep refuses to run in.
const LIVE_NAMESPACE: &str = "smelt-park";

/// What the sweep would delete: each object's name and the uid it was
/// listed with. `kept_claims` counts the old claims it keeps because a pod
/// it keeps still mounts them. `mounts` names each chosen pod's claims, as
/// (pod, claim), so a claim whose pod isn't deleted after all is kept too.
#[derive(Debug, Default, PartialEq)]
struct Choice {
    pods: Vec<(String, String)>,
    claims: Vec<(String, String)>,
    kept_claims: usize,
    mounts: Vec<(String, String)>,
}

/// Refuses unless the sweep's namespace is the one the tests use and isn't
/// the live one.
fn check_namespace(sweep: &str, tests: &str) -> Result<(), String> {
    if sweep == LIVE_NAMESPACE {
        return Err(format!("refusing to sweep {sweep}: it is the live namespace"));
    }
    if sweep != tests {
        return Err(format!("refusing to sweep {sweep}: the tests use {tests}"));
    }
    Ok(())
}

/// How far before the process's own start an object must predate to be
/// taken by the predates arm. The premise is the supported topology:
/// the test process and the API server share one machine and one kernel
/// clock (the compose stack's k3s sidecar), and no correction of 2 s or
/// more lands between boot and process start — `btime` freezes the boot
/// clock's reading, and a later NTP step cannot move it back here. On
/// that premise the two second-floors can shift the comparison by at
/// most 1 s, which this margin outdistances; a start that still reads
/// ahead of the clock judging it turns the arm off (in `choose` and in
/// `process_start_time`), and a `KUBECONFIG` pointing at another
/// machine is outside the supported set (docs/testing.md). A killed
/// run's leftovers predate a re-run's start by far more; this run's own
/// objects never reach it.
const START_MARGIN: SignedDuration = SignedDuration::from_secs(2);

/// The pods and claims to delete: in `SWEEP_NAMESPACE`, not already being
/// deleted, and at least `max_age` old at `now` or — when `process_start`
/// is given — created at least [`START_MARGIN`] before it. An old claim is
/// kept when a pod that isn't chosen and isn't being deleted mounts it.
fn choose(pods: &[Pod], claims: &[PersistentVolumeClaim], now: Timestamp, max_age: Duration, process_start: Option<Timestamp>) -> Choice {
    // A bound too large for a signed duration makes nothing old.
    let max_age = SignedDuration::try_from(max_age).unwrap_or(SignedDuration::MAX);
    // A start that reads ahead of the clock judging it is not a fact
    // about the past but the forward-bias signature (a boot-time clock
    // corrected later: `btime` keeps the old reading) — with it every
    // young object would read as pre-start, so the arm is off.
    let process_start = process_start.filter(|ps| *ps <= now);
    // The name and uid of an object this sweep may delete; `None` for
    // anything outside the namespace, already being deleted, younger than
    // `max_age` and not predating `process_start`, or missing what a
    // guarded delete needs.
    let sweepable = |meta: &ObjectMeta| {
        let ours = meta.namespace.as_deref() == Some(SWEEP_NAMESPACE);
        // Old by the hour, or predating this process (SME-255): under the
        // cluster lock only a finished run's object does that.
        let spent = meta.creation_timestamp.as_ref().is_some_and(|t| {
            now.duration_since(t.0) >= max_age || process_start.is_some_and(|ps| ps.duration_since(t.0) >= START_MARGIN)
        });
        (ours && spent && meta.deletion_timestamp.is_none()).then(|| Some((meta.name.clone()?, meta.uid.clone()?)))?
    };
    let mut choice = Choice::default();
    let mut mounted_by_kept_pods = std::collections::HashSet::new();
    for pod in pods {
        let volumes = pod.spec.as_ref().and_then(|s| s.volumes.as_ref());
        let mounted = volumes.into_iter().flatten().filter_map(|v| v.persistent_volume_claim.as_ref()).map(|c| c.claim_name.as_str());
        if let Some(listed) = sweepable(&pod.metadata) {
            choice.mounts.extend(mounted.map(|claim| (listed.0.clone(), claim.to_string())));
            choice.pods.push(listed);
        } else if pod.metadata.deletion_timestamp.is_none() {
            mounted_by_kept_pods.extend(mounted);
        }
    }
    for claim in claims {
        let Some(listed) = sweepable(&claim.metadata) else { continue };
        if mounted_by_kept_pods.contains(listed.0.as_str()) {
            choice.kept_claims += 1;
        } else {
            choice.claims.push(listed);
        }
    }
    choice
}

/// How many deletes the sweep has in flight at once.
const DELETES_AT_ONCE: usize = 16;

/// What a sweep did: objects it deleted, old claims it kept because a
/// pod it kept or couldn't delete mounts them, and deletes it skipped because the object was
/// already gone (404) or had been created again under its name (409).
#[derive(Debug, Default, PartialEq)]
struct Swept {
    pods: usize,
    claims: usize,
    kept_claims: usize,
    skipped: usize,
    failed: usize,
}

/// The sweep's two `Api`s, built only from `SWEEP_NAMESPACE`, and only
/// once the namespace check passes (guard 2).
fn apis(client: &kube::Client) -> Result<(Api<Pod>, Api<PersistentVolumeClaim>), String> {
    check_namespace(SWEEP_NAMESPACE, crate::sandbox::NAMESPACE)?;
    Ok((Api::namespaced(client.clone(), SWEEP_NAMESPACE), Api::namespaced(client.clone(), SWEEP_NAMESPACE)))
}

/// Lists the namespace's claims and then its pods (only those matching
/// `selector`, when given), and deletes those `choose` picks: pods first,
/// so a claim only they mounted can go once they've stopped.
async fn sweep(client: &kube::Client, max_age: Duration, selector: Option<&str>) -> Result<Swept, String> {
    sweep_with(client, max_age, selector, async {}, Timestamp::now, None).await
}

/// `sweep`, running `between_lists` between its two listings (a test's way
/// to make an object in that window), and judging age at the moment `now`
/// gives once the pods are listed. `sweep` passes the runner's clock; a
/// test can pass a time read from the API server's own timestamps, which
/// are to the second, so that its result doesn't hang on how slowly the
/// runner goes (SME-143). `process_start`, when given, is the predates arm
/// (SME-255): objects made before it by [`START_MARGIN`] are taken however
/// young.
async fn sweep_with(
    client: &kube::Client,
    max_age: Duration,
    selector: Option<&str>,
    between_lists: impl std::future::Future<Output = ()>,
    now: impl FnOnce() -> Timestamp,
    process_start: Option<Timestamp>,
) -> Result<Swept, String> {
    let (pods, claims) = apis(client)?;
    let mut params = ListParams::default();
    if let Some(selector) = selector {
        params = params.labels(selector);
    }
    // Claims first: a pod made after this listing that mounts one of them
    // is then in the pod listing, and keeps it (review 1).
    let listed_claims = claims.list(&params).await.map_err(|e| format!("listing claims: {e}"))?.items;
    between_lists.await;
    let listed_pods = pods.list(&params).await.map_err(|e| format!("listing pods: {e}"))?.items;
    let choice = choose(&listed_pods, &listed_claims, now(), max_age, process_start);
    delete_chosen(client, choice).await
}

/// Deletes each chosen object, held to the uid it was listed with. A
/// chosen claim is kept after all when a pod that mounts it is left (made
/// again under its name, or its delete failed): that pod may still use it
/// (review 2).
async fn delete_chosen(client: &kube::Client, choice: Choice) -> Result<Swept, String> {
    let (pods, claims) = apis(client)?;
    let pods = delete_each(&pods, "pod", choice.pods).await;
    let held: std::collections::HashSet<&str> = choice
        .mounts
        .iter()
        .filter(|(pod, _)| pods.left.contains(pod))
        .map(|(_, claim)| claim.as_str())
        .collect();
    let (kept, to_delete): (Vec<_>, Vec<_>) = choice.claims.into_iter().partition(|(name, _)| held.contains(name.as_str()));
    let claims = delete_each(&claims, "claim", to_delete).await;
    Ok(Swept {
        pods: pods.deleted,
        claims: claims.deleted,
        kept_claims: choice.kept_claims + kept.len(),
        skipped: pods.skipped + claims.skipped,
        failed: pods.failed + claims.failed,
    })
}

#[derive(Default)]
struct Outcome {
    deleted: usize,
    skipped: usize,
    failed: usize,
    /// The objects still there afterwards, as far as the sweep knows: made
    /// again under their name, or their delete failed.
    left: Vec<String>,
}

async fn delete_each<K>(api: &Api<K>, kind: &str, listed: Vec<(String, String)>) -> Outcome
where
    K: Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let results: Vec<_> = stream::iter(listed)
        .map(|(name, uid)| async move {
            let params = DeleteParams {
                preconditions: Some(Preconditions { uid: Some(uid), resource_version: None }),
                ..DeleteParams::default()
            };
            (api.delete(&name, &params).await, name)
        })
        .buffer_unordered(DELETES_AT_ONCE)
        .collect()
        .await;
    let mut outcome = Outcome::default();
    for (result, name) in results {
        match result {
            Ok(_) => outcome.deleted += 1,
            // Its run's teardown or another sweep got there first.
            Err(kube::Error::Api(e)) if e.code == 404 => outcome.skipped += 1,
            // Deleted and made again under the same name since the list:
            // not the object that was old.
            Err(kube::Error::Api(e)) if e.code == 409 => {
                log(format_args!("sweep: {kind} {name} was made again since it was listed; left alone"));
                outcome.skipped += 1;
                outcome.left.push(name);
            }
            Err(e) => {
                log(format_args!("sweep: couldn't delete {kind} {name}: {e}"));
                outcome.failed += 1;
                outcome.left.push(name);
            }
        }
    }
    outcome
}

/// Writes a line straight to stderr, past the test harness's capture, so
/// it shows in a passing run's output too.
fn log(line: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{line}");
}

/// How old a pod or claim must be for the harness's sweep to delete it:
/// far longer than any test keeps one (a scenario is cut off at 300 s, a
/// whole gate takes minutes).
const MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// How long the harness's sweep may take before the test goes on without
/// it.
const SWEEP_BOUND: Duration = Duration::from_secs(60);

/// This process's start, read from `/proc`: field 22 of `/proc/self/stat`
/// (clock ticks since boot, USER_HZ being 100) added to `btime`, the
/// boot's epoch second from `/proc/stat`. `None` if anything reads oddly
/// — the predates arm stays off then and the hour arm goes on
/// unaffected. Second precision only, the floor [`START_MARGIN`]
/// answers. A derived start later than the clock right now is the
/// forward-bias shape [`START_MARGIN`] describes and is refused the
/// same way: no `Some` that would eat this run's own objects.
fn process_start_time() -> Option<Timestamp> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Field 22 sits past the comm field, which can hold spaces and
    // parens: count fields from the last ')'.
    let after = &stat[stat.rfind(')')? + 1..];
    let starttime: u64 = after.split_whitespace().nth(19)?.parse().ok()?;
    let btime: u64 = std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find(|line| line.starts_with("btime "))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    let start = Timestamp::from_second(btime.saturating_add(starttime / 100) as i64).ok()?;
    (start <= Timestamp::now()).then_some(start)
}

static SWEPT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
static SWEEPS_STARTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many times this process has started the harness's sweep.
pub(super) fn sweeps_started() -> usize {
    SWEEPS_STARTED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Sweeps `smelt-park-test` of everything older than `MAX_AGE` or made
/// before this process started (SME-255), once per process: the first
/// caller runs it, any caller while it runs waits for it, and later
/// callers return at once. It has a client of its own, on
/// the first caller's runtime, and gives up after `SWEEP_BOUND`. It never
/// fails the test: whatever happens is one line on stderr.
pub(super) async fn sweep_once() {
    SWEPT
        .get_or_init(|| async {
            SWEEPS_STARTED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let client = crate::sandbox::tests::test_client().await;
            let predates = process_start_time();
            match tokio::time::timeout(SWEEP_BOUND, sweep_with(&client, MAX_AGE, None, async {}, Timestamp::now, predates)).await {
                Ok(Ok(swept)) => log(format_args!(
                    "swept {} pods, {} claims older than 1h or predating this process from {SWEEP_NAMESPACE} ({} kept: mounted by a newer pod; {} skipped, {} failed)",
                    swept.pods, swept.claims, swept.kept_claims, swept.skipped, swept.failed
                )),
                Ok(Err(e)) => log(format_args!("sweep of {SWEEP_NAMESPACE} failed: {e}")),
                Err(_) => log(format_args!("sweep of {SWEEP_NAMESPACE} didn't finish within {SWEEP_BOUND:?}; the next run sweeps again")),
            }
        })
        .await;
}

#[cfg(test)]
mod tests;
