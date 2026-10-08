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
use kube::api::ObjectMeta;

/// The only namespace the sweep lists or deletes in.
const SWEEP_NAMESPACE: &str = "smelt-park-test";

/// The user's live namespace, which the sweep refuses to run in.
const LIVE_NAMESPACE: &str = "smelt-park";

/// What the sweep would delete: each object's name and the uid it was
/// listed with. `kept_claims` counts the old claims it keeps because a pod
/// it keeps still mounts them.
#[derive(Debug, Default, PartialEq)]
struct Choice {
    pods: Vec<(String, String)>,
    claims: Vec<(String, String)>,
    kept_claims: usize,
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
        if let Some(listed) = sweepable(&pod.metadata) {
            choice.pods.push(listed);
        } else if pod.metadata.deletion_timestamp.is_none() {
            let volumes = pod.spec.as_ref().and_then(|s| s.volumes.as_ref());
            mounted_by_kept_pods.extend(
                volumes.into_iter().flatten().filter_map(|v| v.persistent_volume_claim.as_ref()).map(|c| c.claim_name.as_str()),
            );
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
}
