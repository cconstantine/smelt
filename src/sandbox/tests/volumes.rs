use super::*;

#[test]
fn test_docker_pvc_spec_is_named_labelled_and_sized_for_the_conversation() {
    let [(pvc_name, pvc), (workspace_name, workspace)] = conversation_pvc_specs(42, TEST_INSTANCE);
    assert_eq!(workspace_name, "sandbox-workspace-42", "each claim's name comes beside its spec");
    assert_eq!(workspace.metadata.name.as_deref(), Some(workspace_name.as_str()));
    assert_eq!(workspace.metadata.labels, pvc.metadata.labels);
    assert_eq!(pvc_name, "sandbox-docker-42", "each claim's name comes beside its spec");
    assert_eq!(pvc.metadata.name.as_deref(), Some(pvc_name.as_str()));
    assert_eq!(
        pvc.metadata
            .labels
            .as_ref()
            .and_then(|l| l.get(CONVERSATION_LABEL))
            .map(String::as_str),
        Some("42"),
        "the startup sweep finds a claim's conversation by this label"
    );
    let spec = pvc.spec.expect("claim should have a spec");
    assert_eq!(spec.access_modes, Some(vec!["ReadWriteOnce".to_string()]));
    let requested = spec
        .resources
        .and_then(|r| r.requests)
        .and_then(|r| r.get("storage").cloned());
    assert_eq!(requested, Some(Quantity(default_docker_storage_size())));
}

#[test]
fn test_resolve_mount_path_expands_bare_tilde() {
    assert_eq!(resolve_mount_path("~"), "/home/sandbox");
}

#[test]
fn test_resolve_mount_path_expands_tilde_slash_prefix() {
    assert_eq!(resolve_mount_path("~/.ssh"), "/home/sandbox/.ssh");
}

#[test]
fn test_resolve_mount_path_leaves_an_absolute_path_unchanged() {
    assert_eq!(resolve_mount_path("/data/cache"), "/data/cache");
}

#[test]
fn test_resolve_mount_path_leaves_a_non_slash_tilde_prefix_unchanged() {
    // No `~otheruser` support — there's only ever one sandbox user —
    // so this passes through as a literal path rather than expanding.
    assert_eq!(resolve_mount_path("~foo"), "~foo");
}

#[test]
fn test_volume_mounts_for_produces_a_pvc_backed_pair_per_volume() {
    let volumes = vec![
        db::SandboxVolume {
            id: 7,
            name: "ssh-key".to_string(),
            mount_path: "/home/sandbox/.ssh".to_string(),
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        },
        db::SandboxVolume {
            id: 12,
            name: "build-cache".to_string(),
            mount_path: "/data/cache".to_string(),
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        },
    ];

    let (vols, mounts) = volume_mounts_for(&volumes);
    assert_eq!(vols.len(), 2);
    assert_eq!(mounts.len(), 2);

    assert_eq!(vols[0].name, "volume-7");
    assert_eq!(
        vols[0]
            .persistent_volume_claim
            .as_ref()
            .expect("should be PVC-backed")
            .claim_name,
        "sandbox-volume-7"
    );
    assert_eq!(mounts[0].name, "volume-7");
    assert_eq!(mounts[0].mount_path, "/home/sandbox/.ssh");

    assert_eq!(vols[1].name, "volume-12");
    assert_eq!(
        vols[1]
            .persistent_volume_claim
            .as_ref()
            .expect("should be PVC-backed")
            .claim_name,
        "sandbox-volume-12"
    );
    assert_eq!(mounts[1].name, "volume-12");
    assert_eq!(mounts[1].mount_path, "/data/cache");
}

#[test]
fn test_validate_mount_path_accepts_absolute_paths() {
    for path in ["/data", "/home/dev/.cache", "/workspace/project"] {
        assert!(validate_mount_path(path).is_ok(), "{path} should be accepted");
    }
}

#[test]
fn test_validate_mount_path_rejects_relative_empty_and_escaping_paths() {
    for path in ["relative/path", "data", "", "/", "/data/../etc", "../x"] {
        assert!(validate_mount_path(path).is_err(), "{path:?} should be rejected");
    }
}

/// A relative mount path is mounted into every sandbox pod, and a
/// container can't start with one — so it must be refused before it's
/// saved (and before the cluster is touched: MANAGER is unset here).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_create_volume_refuses_a_relative_mount_path_before_saving_it(pool: PgPool) {
    let result = create_volume(&pool, "cache", "relative/path").await;
    assert!(
        matches!(result, Err(SandboxError::InvalidMountPath(_))),
        "expected InvalidMountPath, got {result:?}"
    );
    assert!(
        db::list_sandbox_volumes(&pool).await.expect("list volumes").is_empty(),
        "the refused volume was saved anyway"
    );
}

// SME-239 A-tests, commit 2's share: the claim builder, the mount pair and
// the sweep. (The mount-path helpers land with the tool that calls them, so
// they aren't dead code in a non-test build.)

// --- Model-created volumes: the mount pair, the claim and the sweep (SME-239) ---

#[test]
fn test_build_agent_volume_pvc_spec_is_named_labelled_and_sized() {
    let pvc = build_agent_volume_pvc_spec(7, 4, TEST_INSTANCE);
    assert_eq!(
        pvc.metadata.name.as_deref(),
        Some("agent-volume-7"),
        "not a global volume's `sandbox-volume-7`: the two tables' ids collide"
    );
    let labels = pvc.metadata.labels.as_ref().expect("a claim the sweep lists by label has labels");
    assert_eq!(labels.get(INSTANCE_LABEL).map(String::as_str), Some(TEST_INSTANCE), "mounted only by its own instance (SME-115)");
    assert_eq!(labels.get(AGENT_VOLUME_LABEL).map(String::as_str), Some("4"), "the sweep finds the creating conversation by this label");
    let spec = pvc.spec.expect("claim should have a spec");
    assert_eq!(spec.access_modes, Some(vec!["ReadWriteOnce".to_string()]), "RWO holds on smelt's single-node k3s (the design's spike S2)");
    let requested = spec.resources.and_then(|r| r.requests).and_then(|r| r.get("storage").cloned());
    assert_eq!(requested, Some(Quantity(default_volume_storage_size())), "the setting, not a literal: the check server's .env may set it");
}

#[test]
fn test_agent_volume_mounts_for_names_its_pod_volume_apart_from_a_globals() {
    let agent = db::AgentVolume {
        id: 7,
        owner_conversation_id: 4,
        name: "cache".to_string(),
        mount_path: "/data/cache".to_string(),
    };
    let global = db::SandboxVolume {
        id: 7,
        name: "ssh-key".to_string(),
        mount_path: "/home/sandbox/.ssh".to_string(),
        created_at: chrono::Utc::now().naive_utc(),
        updated_at: chrono::Utc::now().naive_utc(),
    };

    let (vols, mounts) = agent_volume_mounts_for(&[agent]);
    assert_eq!(vols.len(), 1);
    assert_eq!(mounts.len(), 1);
    assert_eq!(vols[0].name, "agent-volume-7");
    assert_eq!(
        vols[0].persistent_volume_claim.as_ref().expect("should be PVC-backed").claim_name,
        "agent-volume-7"
    );
    assert_eq!(mounts[0].name, "agent-volume-7");
    assert_eq!(mounts[0].mount_path, "/data/cache", "the row's path, not a per-conversation one");

    let (global_vols, _) = volume_mounts_for(&[global]);
    assert_ne!(global_vols[0].name, vols[0].name, "one pod mounts both, and their ids come from separate sequences");
}

fn agent_volume_claim(name: &str, owner: Option<&str>, instance: Option<&str>) -> PersistentVolumeClaim {
    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            uid: Some(format!("uid-{name}")),
            labels: Some(
                owner
                    .map(|o| (AGENT_VOLUME_LABEL.to_string(), o.to_string()))
                    .into_iter()
                    .chain(instance.map(|i| (INSTANCE_LABEL.to_string(), i.to_string())))
                    .collect(),
            ),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn test_orphaned_agent_volume_claims_are_the_ones_their_creating_conversation_left() {
    let live = std::collections::HashSet::from([1]);
    let claims = vec![
        agent_volume_claim("agent-volume-1", Some("1"), Some(TEST_INSTANCE)),
        agent_volume_claim("agent-volume-2", Some("2"), Some(TEST_INSTANCE)),
        agent_volume_claim("agent-volume-3", Some("3"), Some("another-database")),
        agent_volume_claim("agent-volume-4", Some("4"), None),
        agent_volume_claim("agent-volume-5", None, Some(TEST_INSTANCE)),
        agent_volume_claim("agent-volume-6", Some("not-a-number"), Some(TEST_INSTANCE)),
    ];
    let orphans = orphaned_agent_volume_claims(&claims, &live, TEST_INSTANCE);
    assert_eq!(
        orphans.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(),
        vec!["agent-volume-2"],
        "only this instance's claim for a conversation the database no longer has"
    );
    assert_eq!(orphans[0].conversation_id, 2, "the owner label names it, not a conversation that merely mounts it");
    assert_eq!(orphans[0].uid, "uid-agent-volume-2", "deleted only while it's still this object");

    let two = vec![
        agent_volume_claim("agent-volume-b", Some("8"), Some(TEST_INSTANCE)),
        agent_volume_claim("agent-volume-a", Some("7"), Some(TEST_INSTANCE)),
    ];
    assert_eq!(
        orphaned_agent_volume_claims(&two, &live, TEST_INSTANCE)
            .iter()
            .map(|o| o.name.as_str())
            .collect::<Vec<_>>(),
        vec!["agent-volume-a", "agent-volume-b"],
        "sorted, so the sweep's log reads the same way every run"
    );
}

/// The path a model volume is stored at: `~` expanded, `.` segments and
/// repeated slashes gone, no trailing slash. What the pod mounts, what the
/// prompt lists and what the next call compares against are one string only
/// if it's normalized before it's stored — Kubernetes takes the path as given,
/// and smelt compares paths as text (SME-239).
#[test]
fn test_model_volume_mount_path_normalizes_and_resolves() {
    assert_eq!(model_volume_mount_path("/data//cache/").expect("ok"), "/data/cache");
    assert_eq!(model_volume_mount_path("/data/./cache").expect("ok"), "/data/cache");
    assert_eq!(model_volume_mount_path("/data/cache///").expect("ok"), "/data/cache");
    assert_eq!(model_volume_mount_path("~/cache").expect("ok"), "/home/sandbox/cache");
    assert_eq!(model_volume_mount_path("~").expect("ok"), "/home/sandbox");
    // Normalizing doesn't make a relative path absolute, and `..` survives it,
    // so both keep the global volume's own refusal words.
    assert_eq!(model_volume_mount_path("cache").unwrap_err(), "\"cache\" must be an absolute path (start with / or ~)");
    assert_eq!(model_volume_mount_path("/data/../cache").unwrap_err(), "\"/data/../cache\" must not contain ..");
    assert_eq!(model_volume_mount_path("/").unwrap_err(), "a volume can't be mounted over the root directory");
}

/// The directories a model volume can't be mounted at or under: the design's
/// list, plus the pod's Docker socket — a volume over which would break every
/// command the pod runs through its sidecar.
#[test]
fn test_model_volume_mount_path_refuses_reserved_directories() {
    for path in [
        "/workspace",
        "/workspace/repo",
        "/etc/smelt",
        "/etc/smelt/keys/id_ed25519",
        "/run/docker-sock",
        "/run/docker-sock/docker.sock",
        "/etc/ssl",
        "/usr/local",
        "/bin",
        "/lib",
        "/proc/1",
        "/sys",
        "/dev/shm",
        "/var/lib/docker/overlay2",
        "/etc/smelt-x",
    ] {
        assert!(model_volume_mount_path(path).is_err(), "{path} is a reserved mount path");
    }
    // Named by the narrowest match, since that's the reason it's refused.
    assert_eq!(
        model_volume_mount_path("/etc/smelt/keys").unwrap_err(),
        "\"/etc/smelt/keys\" is at or under \"/etc/smelt\", which a volume can't be mounted over"
    );
    // What merely sits next to a reserved directory isn't under it. (`/etc/…`
    // itself always is: `/etc` is on the list.)
    for allowed in ["/run/docker-sock-x", "/var/lib/dockerx", "/workspace-x", "/data/cache"] {
        assert_eq!(model_volume_mount_path(allowed).expect("not reserved"), allowed);
    }
}

/// `/etc/smelt` is a literal in spec's reserved list, which keeps that module
/// (test-only here) free of `crate::git` in production. It has to stay equal to the directory the pod's SSH
/// key is written into, or a model volume could carry that key onto a claim
/// another conversation mounts (SME-239).
#[test]
fn test_reserved_git_dir_literal_is_pods_git_dir() {
    assert_eq!(
        model_volume_mount_path(crate::git::POD_GIT_DIR).unwrap_err(),
        "\"/etc/smelt\" is at or under \"/etc/smelt\", which a volume can't be mounted over",
        "POD_GIT_DIR moved; the reserved list has to move with it"
    );
}
