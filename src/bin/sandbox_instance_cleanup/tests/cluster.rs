use super::*;

fn labels(instance: Option<&str>) -> serde_json::Map<String, serde_json::Value> {
    let mut labels = serde_json::Map::new();
    if let Some(instance) = instance {
        labels.insert(INSTANCE_LABEL.to_string(), instance.to_string().into());
    }
    labels
}

/// A pod that never starts (an image no node has, never pulled),
/// which is enough to delete.
fn pod(name: &str, instance: Option<&str>) -> Pod {
    serde_json::from_value(serde_json::json!({
        "metadata": {"name": name, "labels": labels(instance)},
        "spec": {
            "containers": [{
                "name": "sandbox",
                "image": "smelt.invalid/none:0",
                "imagePullPolicy": "Never",
            }],
        },
    }))
    .expect("a pod spec")
}

fn claim(name: &str, instance: Option<&str>) -> PersistentVolumeClaim {
    serde_json::from_value(serde_json::json!({
        "metadata": {"name": name, "labels": labels(instance)},
        "spec": {
            "accessModes": ["ReadWriteOnce"],
            "resources": {"requests": {"storage": "1Mi"}},
        },
    }))
    .expect("a claim spec")
}

/// Cleanup deletes exactly the instance it is given: everything of
/// that instance and nothing else. Another instance's objects and
/// the unlabelled ones keep their uids.
#[tokio::test]
async fn test_cleanup_deletes_only_the_instance_it_is_given() {
    let client = kube::Client::try_default().await.expect("a cluster client");
    let pods: Api<Pod> = Api::namespaced(client.clone(), NAMESPACE);
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), NAMESPACE);
    let nonce = unique();
    let ours = format!("cleanup-a-{nonce}");
    let theirs = format!("cleanup-b-{nonce}");
    let named = |kind: &str| format!("cleanup-{kind}-{nonce}");

    let ours_pods = [named("a1"), named("a2")];
    let ours_claims = [named("aclaim1"), named("aclaim2")];
    let theirs_pod = named("b1");
    let theirs_claim = named("bclaim1");
    let unlabelled_pod = named("unlabelled1");
    let unlabelled_claim = named("unlabelledclaim1");

    let mut made_pods: Vec<String> = Vec::new();
    for (name, instance) in ours_pods
        .iter()
        .map(|name| (name.as_str(), Some(ours.as_str())))
        .chain([
            (theirs_pod.as_str(), Some(theirs.as_str())),
            (unlabelled_pod.as_str(), None),
        ])
    {
        pods.create(&Default::default(), &pod(name, instance))
            .await
            .expect("create a pod");
        made_pods.push(name.to_string());
    }
    let mut made_claims: Vec<String> = Vec::new();
    for (name, instance) in ours_claims
        .iter()
        .map(|name| (name.as_str(), Some(ours.as_str())))
        .chain([
            (theirs_claim.as_str(), Some(theirs.as_str())),
            (unlabelled_claim.as_str(), None),
        ])
    {
        claims
            .create(&Default::default(), &claim(name, instance))
            .await
            .expect("create a claim");
        made_claims.push(name.to_string());
    }

    let theirs_pod_uid = pods.get(&theirs_pod).await.expect("their pod").metadata.uid;
    let theirs_claim_uid = claims
        .get(&theirs_claim)
        .await
        .expect("their claim")
        .metadata
        .uid;
    let unlabelled_pod_uid = pods
        .get(&unlabelled_pod)
        .await
        .expect("the unlabelled pod")
        .metadata
        .uid;
    let unlabelled_claim_uid = claims
        .get(&unlabelled_claim)
        .await
        .expect("the unlabelled claim")
        .metadata
        .uid;

    let finished = delete_instance(&client, &ours, false)
        .await
        .expect("the cleanup");

    assert!(
        finished,
        "the cleanup must finish with only the given instance's objects there"
    );
    for name in ours_pods.iter().chain(ours_claims.iter()) {
        assert!(
            pods.get_opt(name).await.expect("get").is_none()
                && claims.get_opt(name).await.expect("get").is_none(),
            "{name} should have been deleted"
        );
    }
    assert_eq!(
        pods.get(&theirs_pod).await.expect("their pod").metadata.uid,
        theirs_pod_uid,
        "another instance's pod must be kept"
    );
    assert_eq!(
        claims.get(&theirs_claim).await.expect("their claim").metadata.uid,
        theirs_claim_uid,
        "another instance's claim must be kept"
    );
    assert_eq!(
        pods.get(&unlabelled_pod)
            .await
            .expect("the unlabelled pod")
            .metadata
            .uid,
        unlabelled_pod_uid,
        "an unlabelled pod must be kept"
    );
    assert_eq!(
        claims.get(&unlabelled_claim)
            .await
            .expect("the unlabelled claim")
            .metadata
            .uid,
        unlabelled_claim_uid,
        "an unlabelled claim must be kept"
    );

    // Last, so the assertions above saw what was kept: delete
    // everything this test made. A killed run leaves them, which is
    // harmless — every name is unique to this run.
    for name in &made_pods {
        let _ = pods
            .delete(
                name,
                &DeleteParams {
                    grace_period_seconds: Some(0),
                    ..Default::default()
                },
            )
            .await;
    }
    for name in &made_claims {
        let _ = claims.delete(name, &DeleteParams::default()).await;
    }
}
