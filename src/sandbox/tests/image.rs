use super::*;

/// A server with no `SANDBOX_IMAGE` runs the image named after its own
/// agent sources, the one `scripts/sandbox-image-ref` prints and
/// `scripts/build-sandbox-image.sh` builds, never a `:latest` that only a
/// manual rebuild moves (SME-121). Compares with the script itself, so
/// `build.rs`'s hash can't drift from it.
#[test]
fn test_with_no_setting_the_image_is_the_one_named_after_this_trees_agent_sources() {
    let out = std::process::Command::new("scripts/sandbox-image-ref").output().expect("run scripts/sandbox-image-ref");
    assert!(out.status.success(), "scripts/sandbox-image-ref failed: {}", String::from_utf8_lossy(&out.stderr));
    let script = String::from_utf8(out.stdout).expect("utf-8");
    assert_eq!(sandbox_image_from(None), script.trim());
    assert_eq!(sandbox_image_from(Some(String::new())), script.trim(), "an empty setting is no setting");
}

#[test]
fn test_a_sandbox_image_setting_names_the_image() {
    let image = "docker.io/library/smelt-sandbox:latest";
    assert_eq!(sandbox_image_from(Some(image.to_string())), image);
}

/// What `ClusterDialer::image` reads from a refused pod is the reference
/// `create_pod` gave it, so a pod made from the current image compares
/// equal to the one a new pod gets (SME-121).
#[test]
fn test_a_pods_sandbox_image_reads_back_as_the_one_it_was_built_with() {
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[], TEST_INSTANCE);
    assert_eq!(sandbox_container_image(&pod), Some(default_sandbox_image()));
    assert_eq!(sandbox_container_image(&Pod::default()), None);
}
