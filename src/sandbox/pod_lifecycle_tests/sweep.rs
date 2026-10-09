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

/// The pods and claims to delete: in `SWEEP_NAMESPACE`, not already being
/// deleted, and at least `max_age` old at `now`. An old claim is kept when
/// a pod that isn't chosen and isn't being deleted mounts it.
fn choose(pods: &[Pod], claims: &[PersistentVolumeClaim], now: Timestamp, max_age: Duration) -> Choice {
    // A bound too large for a signed duration makes nothing old.
    let max_age = SignedDuration::try_from(max_age).unwrap_or(SignedDuration::MAX);
    // The name and uid of an object this sweep may delete; `None` for
    // anything outside the namespace, already being deleted, younger than
    // `max_age`, or missing what a guarded delete needs.
    let sweepable = |meta: &ObjectMeta| {
        let ours = meta.namespace.as_deref() == Some(SWEEP_NAMESPACE);
        let old = meta.creation_timestamp.as_ref().is_some_and(|t| now.duration_since(t.0) >= max_age);
        (ours && old && meta.deletion_timestamp.is_none()).then(|| Some((meta.name.clone()?, meta.uid.clone()?)))?
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
    sweep_with(client, max_age, selector, async {}).await
}

/// `sweep`, running `between_lists` between its two listings: a test's way
/// to make an object in that window.
async fn sweep_with(
    client: &kube::Client,
    max_age: Duration,
    selector: Option<&str>,
    between_lists: impl std::future::Future<Output = ()>,
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
    let choice = choose(&listed_pods, &listed_claims, Timestamp::now(), max_age);
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

static SWEPT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
static SWEEPS_STARTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many times this process has started the harness's sweep.
pub(super) fn sweeps_started() -> usize {
    SWEEPS_STARTED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Sweeps `smelt-park-test` of everything older than `MAX_AGE`, once per
/// process: the first caller runs it, any caller while it runs waits for
/// it, and later callers return at once. It has a client of its own, on
/// the first caller's runtime, and gives up after `SWEEP_BOUND`. It never
/// fails the test: whatever happens is one line on stderr.
pub(super) async fn sweep_once() {
    SWEPT
        .get_or_init(|| async {
            SWEEPS_STARTED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let client = crate::sandbox::tests::test_client().await;
            match tokio::time::timeout(SWEEP_BOUND, sweep(&client, MAX_AGE, None)).await {
                Ok(Ok(swept)) => log(format_args!(
                    "swept {} pods, {} claims older than 1h from {SWEEP_NAMESPACE} ({} kept: mounted by a newer pod; {} skipped, {} failed)",
                    swept.pods, swept.claims, swept.kept_claims, swept.skipped, swept.failed
                )),
                Ok(Err(e)) => log(format_args!("sweep of {SWEEP_NAMESPACE} failed: {e}")),
                Err(_) => log(format_args!("sweep of {SWEEP_NAMESPACE} didn't finish within {SWEEP_BOUND:?}; the next run sweeps again")),
            }
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(60 * 60);

    fn now() -> Timestamp {
        "2026-10-08T12:00:00Z".parse().expect("timestamp")
    }

    /// `age` before `now()`.
    fn created(age: Duration) -> String {
        let age = SignedDuration::try_from(age).expect("age");
        now().checked_sub(age).expect("time").to_string()
    }

    fn meta(name: &str, namespace: Option<&str>, age: Duration, labels: serde_json::Value) -> serde_json::Value {
        let mut meta = serde_json::json!({
            "name": name,
            "uid": format!("uid-{name}"),
            "creationTimestamp": created(age),
            "labels": labels,
        });
        if let Some(namespace) = namespace {
            meta["namespace"] = namespace.into();
        }
        meta
    }

    fn deleting(mut meta: serde_json::Value) -> serde_json::Value {
        meta["deletionTimestamp"] = now().to_string().into();
        meta
    }

    fn pod_from(meta: serde_json::Value, mounts: &[&str]) -> Pod {
        let volumes: Vec<_> = mounts
            .iter()
            .enumerate()
            .map(|(i, claim)| serde_json::json!({"name": format!("v{i}"), "persistentVolumeClaim": {"claimName": claim}}))
            .collect();
        serde_json::from_value(serde_json::json!({
            "metadata": meta,
            "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}], "volumes": volumes},
        }))
        .expect("pod")
    }

    fn claim_from(meta: serde_json::Value) -> PersistentVolumeClaim {
        serde_json::from_value(serde_json::json!({"metadata": meta})).expect("claim")
    }

    fn pod(name: &str, age: Duration, mounts: &[&str]) -> Pod {
        pod_from(meta(name, Some(SWEEP_NAMESPACE), age, serde_json::json!({})), mounts)
    }

    fn claim(name: &str, age: Duration) -> PersistentVolumeClaim {
        claim_from(meta(name, Some(SWEEP_NAMESPACE), age, serde_json::json!({})))
    }

    fn listed(name: &str) -> (String, String) {
        (name.to_string(), format!("uid-{name}"))
    }

    /// An object at least `max_age` old is chosen, a younger one isn't;
    /// exactly `max_age` counts as old.
    #[test]
    fn test_the_sweep_chooses_objects_at_least_max_age_old() {
        let pods = [
            pod("old", 2 * HOUR, &[]),
            pod("exactly", HOUR, &[]),
            pod("young", HOUR - Duration::from_secs(1), &[]),
        ];
        let claims = [claim("old-claim", 2 * HOUR), claim("young-claim", Duration::from_secs(60))];

        let choice = choose(&pods, &claims, now(), HOUR);

        assert_eq!(choice.pods, vec![listed("old"), listed("exactly")]);
        assert_eq!(choice.claims, vec![listed("old-claim")]);
        assert_eq!(choice.kept_claims, 0);
    }

    /// A pod or claim already being deleted is left to Kubernetes, however
    /// old: `test_a_stopping_pod_from_another_run_doesnt_hold_up_the_tier`
    /// holds one in Terminating on purpose.
    #[test]
    fn test_the_sweep_never_chooses_an_object_already_being_deleted() {
        let pods = [pod_from(deleting(meta("stopping", Some(SWEEP_NAMESPACE), 5 * HOUR, serde_json::json!({}))), &[])];
        let claims = [claim_from(deleting(meta("going", Some(SWEEP_NAMESPACE), 5 * HOUR, serde_json::json!({}))))];

        assert_eq!(choose(&pods, &claims, now(), HOUR), Choice::default());
    }

    /// An old claim a young pod mounts is kept, and counted; one mounted
    /// only by a pod the sweep deletes, or by one already being deleted,
    /// is chosen.
    #[test]
    fn test_the_sweep_keeps_an_old_claim_a_kept_pod_mounts() {
        let pods = [
            pod("young-pod", Duration::from_secs(60), &["held"]),
            pod("old-pod", 2 * HOUR, &["mounted-by-old"]),
            pod_from(deleting(meta("stopping", Some(SWEEP_NAMESPACE), 2 * HOUR, serde_json::json!({}))), &["mounted-by-stopping"]),
        ];
        let claims = [
            claim("held", 3 * HOUR),
            claim("mounted-by-old", 3 * HOUR),
            claim("mounted-by-stopping", 3 * HOUR),
        ];

        let choice = choose(&pods, &claims, now(), HOUR);

        assert_eq!(choice.pods, vec![listed("old-pod")]);
        assert_eq!(choice.claims, vec![listed("mounted-by-old"), listed("mounted-by-stopping")]);
        assert_eq!(choice.kept_claims, 1);
        assert_eq!(choice.mounts, vec![("old-pod".to_string(), "mounted-by-old".to_string())]);
    }

    /// The tests' fixed instance, a database's UUID or no label at all:
    /// only age decides.
    #[test]
    fn test_the_sweep_ignores_instance_labels() {
        let labels = [
            serde_json::json!({"smelt/instance": "smelt-tests"}),
            serde_json::json!({"smelt/instance": "0f8e2a5c-1b2d-4e6f-8a9b-0c1d2e3f4a5b"}),
            serde_json::json!({}),
        ];
        let mut pods = Vec::new();
        let mut claims = Vec::new();
        for (i, labels) in labels.iter().enumerate() {
            for (age, which) in [(2 * HOUR, "old"), (Duration::from_secs(60), "young")] {
                pods.push(pod_from(meta(&format!("{which}-pod-{i}"), Some(SWEEP_NAMESPACE), age, labels.clone()), &[]));
                claims.push(claim_from(meta(&format!("{which}-claim-{i}"), Some(SWEEP_NAMESPACE), age, labels.clone())));
            }
        }

        let choice = choose(&pods, &claims, now(), HOUR);

        assert_eq!(choice.pods, (0..3).map(|i| listed(&format!("old-pod-{i}"))).collect::<Vec<_>>());
        assert_eq!(choice.claims, (0..3).map(|i| listed(&format!("old-claim-{i}"))).collect::<Vec<_>>());
    }

    /// Guard 3: an object listed from `smelt-park`, or with no namespace,
    /// is never chosen, however old, even if a wrongly built `Api` handed
    /// it over.
    #[test]
    fn test_the_sweep_never_chooses_an_object_outside_the_test_namespace() {
        let ancient = 1000 * HOUR;
        let pods = [
            pod_from(meta("sandbox-1", Some("smelt-park"), ancient, serde_json::json!({})), &[]),
            pod_from(meta("sandbox-2", None, ancient, serde_json::json!({})), &[]),
            pod_from(meta("sandbox-3", Some("default"), ancient, serde_json::json!({})), &[]),
        ];
        let claims = [
            claim_from(meta("sandbox-workspace-1", Some("smelt-park"), ancient, serde_json::json!({}))),
            claim_from(meta("sandbox-workspace-2", None, ancient, serde_json::json!({}))),
        ];

        assert_eq!(choose(&pods, &claims, now(), HOUR), Choice::default());
    }

    /// Guards 2 and 3 name only the tests' namespace: the literal is
    /// `smelt-park-test`, the same as the harness's, and the entry check
    /// refuses the live namespace or any mismatch.
    #[test]
    fn test_the_sweep_names_only_the_test_namespace() {
        assert_eq!(SWEEP_NAMESPACE, "smelt-park-test");
        assert_eq!(SWEEP_NAMESPACE, crate::sandbox::NAMESPACE);
        assert_eq!(LIVE_NAMESPACE, "smelt-park");
        assert_eq!(check_namespace(SWEEP_NAMESPACE, crate::sandbox::NAMESPACE), Ok(()));
        assert!(check_namespace("smelt-park", "smelt-park").is_err(), "the live namespace must be refused");
        assert!(check_namespace("smelt-park-test", "smelt-park").is_err(), "a mismatch with the harness must be refused");
        assert!(check_namespace("smelt-park", "smelt-park-test").is_err(), "a mismatch with the harness must be refused");
    }

    /// The real-cluster tests, in a module of their own so a filter on
    /// the pure tests' path (`sweep::tests::test_`) can't match them.
    mod cluster {
        use super::*;

        // --- Against the real cluster, in `smelt-park-test`. Each test labels
        // what it makes with a value of its own and sweeps only that label, so
        // even `max_age` zero reaches nothing of another test's or run's.

        /// A label of this test's own, and the selector that picks it.
        /// The clock alone isn't enough: two tests in parallel once got the
        /// same nanoseconds and made the same claim name. A counter keeps
        /// this process's apart, and the process id other processes'.
        fn scope() -> (String, String) {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let run = format!("{}-{}-{n}", crate::sandbox::tests::unique_session_id("sweep"), std::process::id());
            let selector = format!("{SWEEP_TEST_LABEL}={run}");
            (run, selector)
        }

        const SWEEP_TEST_LABEL: &str = "smelt/sweep-test";

        async fn create_claim(client: &kube::Client, name: &str, run: &str) -> PersistentVolumeClaim {
            let claim: PersistentVolumeClaim = serde_json::from_value(serde_json::json!({
                "metadata": {"name": name, "labels": {SWEEP_TEST_LABEL: run}},
                "spec": {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "1Mi"}}},
            }))
            .expect("claim");
            Api::<PersistentVolumeClaim>::namespaced(client.clone(), SWEEP_NAMESPACE)
                .create(&PostParams::default(), &claim)
                .await
                .expect("create claim")
        }

        /// A pod that mounts `claim` and never schedules (no node matches its
        /// selector), so nothing is pulled, started or provisioned.
        async fn create_pod(client: &kube::Client, name: &str, run: &str, claim: &str) {
            let pod: Pod = serde_json::from_value(serde_json::json!({
                "metadata": {"name": name, "labels": {SWEEP_TEST_LABEL: run}},
                "spec": {
                    "nodeSelector": {SWEEP_TEST_LABEL: "no-such-node"},
                    "containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}],
                    "volumes": [{"name": "v", "persistentVolumeClaim": {"claimName": claim}}],
                },
            }))
            .expect("pod");
            Api::<Pod>::namespaced(client.clone(), SWEEP_NAMESPACE)
                .create(&PostParams::default(), &pod)
                .await
                .expect("create pod");
        }

        /// The names of `selector`'s pods and claims not being deleted.
        async fn live(client: &kube::Client, selector: &str) -> Vec<String> {
            let params = ListParams::default().labels(selector);
            let pods = Api::<Pod>::namespaced(client.clone(), SWEEP_NAMESPACE).list(&params).await.expect("list pods");
            let claims =
                Api::<PersistentVolumeClaim>::namespaced(client.clone(), SWEEP_NAMESPACE).list(&params).await.expect("list claims");
            let metas = pods.items.into_iter().map(|p| p.metadata).chain(claims.items.into_iter().map(|c| c.metadata));
            let mut names: Vec<String> = metas.filter(|m| m.deletion_timestamp.is_none()).filter_map(|m| m.name).collect();
            names.sort();
            names
        }

        /// Deletes everything `selector` picks (the role has no
        /// `deletecollection`, so one by one).
        async fn clean_up(client: &kube::Client, selector: &str) {
            let params = ListParams::default().labels(selector);
            let pods = Api::<Pod>::namespaced(client.clone(), SWEEP_NAMESPACE);
            let now = DeleteParams { grace_period_seconds: Some(0), ..DeleteParams::default() };
            for name in pods.list(&params).await.map(|l| l.items).unwrap_or_default().into_iter().filter_map(|p| p.metadata.name) {
                pods.delete(&name, &now).await.ok();
            }
            let claims = Api::<PersistentVolumeClaim>::namespaced(client.clone(), SWEEP_NAMESPACE);
            for name in claims.list(&params).await.map(|l| l.items).unwrap_or_default().into_iter().filter_map(|c| c.metadata.name) {
                claims.delete(&name, &DeleteParams::default()).await.ok();
            }
        }

        /// Old pods and claims are deleted, a claim an old pod mounts with
        /// them; young ones, and a young claim a young pod mounts, are kept.
        #[tokio::test]
        async fn test_the_sweep_deletes_old_pods_and_claims_and_keeps_young_ones() {
            let client = crate::sandbox::tests::test_client().await;
            let (run, selector) = scope();
            create_claim(&client, &format!("{run}-alone"), &run).await;
            create_claim(&client, &format!("{run}-mounted"), &run).await;
            create_pod(&client, &format!("{run}-pod"), &run, &format!("{run}-mounted")).await;

            // Everything is old to a zero age.
            let old = sweep(&client, Duration::ZERO, Some(&selector)).await;
            let left_by_old = live(&client, &selector).await;

            let (young_run, young_selector) = scope();
            create_claim(&client, &format!("{young_run}-claim"), &young_run).await;
            create_pod(&client, &format!("{young_run}-pod"), &young_run, &format!("{young_run}-claim")).await;
            let young = sweep(&client, Duration::from_secs(60 * 60), Some(&young_selector)).await;
            let left_by_young = live(&client, &young_selector).await;

            clean_up(&client, &selector).await;
            clean_up(&client, &young_selector).await;
            assert_eq!(old, Ok(Swept { pods: 1, claims: 2, ..Swept::default() }));
            assert_eq!(left_by_old, Vec::<String>::new(), "the old objects should be deleted or deleting");
            assert_eq!(young, Ok(Swept::default()));
            assert_eq!(left_by_young, vec![format!("{young_run}-claim"), format!("{young_run}-pod")]);
        }

        /// SME-134 review 1: a pod made between the sweep's two listings,
        /// mounting an old claim the sweep lists, is seen with it, and the
        /// young pod keeps the claim: it isn't deleted from under a pod the
        /// sweep doesn't know about. (Review 2: an old claim and a young pod,
        /// the case itself, rather than both old.)
        #[tokio::test]
        async fn test_the_sweep_sees_a_pod_made_between_its_listings() {
            let client = crate::sandbox::tests::test_client().await;
            let (run, selector) = scope();
            let claim_name = format!("{run}-claim");
            let pod_name = format!("{run}-pod");
            create_claim(&client, &claim_name, &run).await;
            // Creation times are to the second: the claim is then over a
            // second old, and the pod made during the sweep under one.
            tokio::time::sleep(Duration::from_secs(2)).await;

            let made_between =
                sweep_with(&client, Duration::from_secs(1), Some(&selector), create_pod(&client, &pod_name, &run, &claim_name));
            let swept = made_between.await;
            let left = live(&client, &selector).await;

            clean_up(&client, &selector).await;
            assert_eq!(left, vec![claim_name, pod_name], "the claim was deleted from under a pod the sweep didn't see");
            assert_eq!(swept, Ok(Swept { kept_claims: 1, ..Swept::default() }));
        }

        /// SME-134 review 2: a claim the sweep chose with its old pod is kept
        /// when that pod's delete is refused, because the pod was made again
        /// under its name since the listing and still mounts the claim.
        #[tokio::test]
        async fn test_the_sweep_keeps_a_claim_whose_pod_was_made_again() {
            let client = crate::sandbox::tests::test_client().await;
            let (run, selector) = scope();
            let claim_name = format!("{run}-claim");
            let pod_name = format!("{run}-pod");
            let claim = create_claim(&client, &claim_name, &run).await;
            create_pod(&client, &pod_name, &run, &claim_name).await;
            let pods = Api::<Pod>::namespaced(client.clone(), SWEEP_NAMESPACE);
            let listed_pod_uid = pods.get(&pod_name).await.expect("get pod").metadata.uid.expect("uid");
            pods.delete(&pod_name, &DeleteParams { grace_period_seconds: Some(0), ..DeleteParams::default() })
                .await
                .expect("delete pod");
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            while pods.get_opt(&pod_name).await.expect("get").is_some() {
                assert!(tokio::time::Instant::now() < deadline, "the first pod wasn't gone within 30 s");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            create_pod(&client, &pod_name, &run, &claim_name).await;

            let choice = Choice {
                pods: vec![(pod_name.clone(), listed_pod_uid)],
                claims: vec![(claim_name.clone(), claim.metadata.uid.clone().expect("uid"))],
                mounts: vec![(pod_name.clone(), claim_name.clone())],
                ..Choice::default()
            };
            let swept = delete_chosen(&client, choice).await;
            let left = live(&client, &selector).await;

            clean_up(&client, &selector).await;
            assert_eq!(left, vec![claim_name, pod_name], "the pod made again, and the claim it mounts, should both be left");
            assert_eq!(swept, Ok(Swept { kept_claims: 1, skipped: 1, ..Swept::default() }));
        }

        /// A claim deleted and made again under its name between the listing
        /// and the delete has a new uid: the delete held to the listed uid is
        /// refused and skipped, and the new claim stays. One already gone is
        /// skipped too.
        #[tokio::test]
        async fn test_the_sweep_skips_an_object_recreated_under_its_name() {
            let client = crate::sandbox::tests::test_client().await;
            let (run, selector) = scope();
            let name = format!("{run}-claim");
            let claims = Api::<PersistentVolumeClaim>::namespaced(client.clone(), SWEEP_NAMESPACE);
            let first = create_claim(&client, &name, &run).await;
            let listed_uid = first.metadata.uid.clone().expect("uid");
            claims.delete(&name, &DeleteParams::default()).await.expect("delete");
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            while claims.get_opt(&name).await.expect("get").is_some() {
                assert!(tokio::time::Instant::now() < deadline, "the first claim wasn't gone within 30 s");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            let again = create_claim(&client, &name, &run).await;

            let choice = Choice {
                claims: vec![(name.clone(), listed_uid), (format!("{run}-never-made"), "no-such-uid".to_string())],
                ..Choice::default()
            };
            let swept = delete_chosen(&client, choice).await;
            let after = claims.get_opt(&name).await.expect("get");

            clean_up(&client, &selector).await;
            assert_eq!(swept, Ok(Swept { skipped: 2, ..Swept::default() }));
            let after = after.expect("the claim made again should still be there");
            assert_eq!(after.metadata.uid, again.metadata.uid);
            assert!(after.metadata.deletion_timestamp.is_none(), "the claim made again was marked for deletion");
        }
    }
}
