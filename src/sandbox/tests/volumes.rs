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
#[sqlx::test]
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
