use super::*;

#[test]
fn test_ownership_tells_ours_from_unlabelled_and_foreign() {
    assert_eq!(ownership(&meta_with_instance(Some("ours")), "ours"), Ownership::Ours);
    assert_eq!(ownership(&meta_with_instance(None), "ours"), Ownership::Unlabelled);
    assert_eq!(ownership(&ObjectMeta::default(), "ours"), Ownership::Unlabelled, "no labels at all");
    assert_eq!(ownership(&meta_with_instance(Some("theirs")), "ours"), Ownership::Foreign);
    // An instance that was never read must not match an empty label.
    assert_eq!(ownership(&meta_with_instance(Some("")), ""), Ownership::Foreign);
}

fn claim(name: &str, conversation: Option<&str>) -> PersistentVolumeClaim {
    claim_of(name, conversation, Some(TEST_INSTANCE))
}

fn claim_of(name: &str, conversation: Option<&str>, instance: Option<&str>) -> PersistentVolumeClaim {
    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            uid: Some(format!("uid-{name}")),
            labels: Some(
                conversation
                    .map(|c| (CONVERSATION_LABEL.to_string(), c.to_string()))
                    .into_iter()
                    .chain(instance.map(|i| (INSTANCE_LABEL.to_string(), i.to_string())))
                    .collect(),
            ),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn names(orphans: &[OrphanedClaim]) -> Vec<&str> {
    orphans.iter().map(|o| o.name.as_str()).collect()
}

#[test]
fn test_orphaned_docker_claims_are_those_whose_conversation_is_gone() {
    let claims = vec![
        claim("sandbox-docker-1", Some("1")),
        claim("sandbox-docker-2", Some("2")),
        // A sandbox volume's claim has no conversation label.
        claim("sandbox-volume-3", None),
        claim("sandbox-docker-x", Some("not-a-number")),
    ];
    let live = std::collections::HashSet::from([1]);
    let orphans = orphaned_docker_claims(&claims, &live, TEST_INSTANCE);
    assert_eq!(names(&orphans), vec!["sandbox-docker-2"]);
    assert_eq!(orphans[0].conversation_id, 2);
    assert_eq!(orphans[0].uid, "uid-sandbox-docker-2", "deleted only while it's still this object");
    // A conversation has two claims (Docker data and /workspace): both go.
    let both = vec![
        claim("sandbox-docker-4", Some("4")),
        claim("sandbox-workspace-4", Some("4")),
    ];
    assert_eq!(
        names(&orphaned_docker_claims(&both, &live, TEST_INSTANCE)),
        vec!["sandbox-docker-4", "sandbox-workspace-4"]
    );
}

/// SME-115, the bug: a server on an empty database (a check server's
/// scratch one) took every conversation's claims in the namespace for
/// orphans. Only its own are; one from before the fix (no instance) and
/// another database's never are.
#[test]
fn test_only_our_own_claims_are_ever_orphans() {
    let claims = vec![
        claim_of("sandbox-workspace-1528", Some("1528"), None),
        claim_of("sandbox-docker-1528", Some("1528"), Some("the-dev-database")),
        claim_of("sandbox-workspace-7", Some("7"), Some("a-scratch-database")),
    ];
    let nothing_live = std::collections::HashSet::new();
    assert_eq!(
        names(&orphaned_docker_claims(&claims, &nothing_live, "a-scratch-database")),
        vec!["sandbox-workspace-7"]
    );
    assert!(orphaned_docker_claims(&claims, &nothing_live, "yet-another").is_empty());
    assert!(orphaned_docker_claims(&claims, &nothing_live, "").is_empty(), "an unread instance owns nothing");
}
