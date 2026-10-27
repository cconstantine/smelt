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
    let pod = build_pod_spec("sandbox-1", "1Gi", &docker_for_conversation(42), &[], &[], TEST_INSTANCE);
    assert_eq!(sandbox_container_image(&pod), Some(default_sandbox_image()));
    assert_eq!(sandbox_container_image(&Pod::default()), None);
}

/// Every file the agent is built from names its image (SME-163): each
/// `.rs` file under `src/bin/sandbox_agent/`, and each file one of them
/// compiles in with `#[path]`. `build.rs` hashes the same files (the test
/// above compares the two), so an edit to any of them renames the image.
#[test]
fn test_the_image_hash_covers_every_file_of_the_agent() {
    let out = std::process::Command::new("scripts/agent-sources-hash")
        .arg("--list")
        .output()
        .expect("run scripts/agent-sources-hash --list");
    assert!(out.status.success(), "agent-sources-hash --list failed: {}", String::from_utf8_lossy(&out.stderr));
    let listed: Vec<String> = String::from_utf8(out.stdout).expect("utf-8").lines().map(str::to_string).collect();
    let mut agent = Vec::new();
    let mut dirs = vec![std::path::PathBuf::from("src/bin/sandbox_agent")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).expect("read the agent's directory") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                agent.push(path);
            }
        }
    }
    assert!(agent.len() > 1, "the agent's files: {agent:?}");
    for file in &agent {
        let name = file.to_string_lossy().to_string();
        assert!(listed.contains(&name), "{name} isn't among the image's sources: {listed:?}");
        let text = std::fs::read_to_string(file).expect("read an agent file");
        for line in text.lines() {
            let Some(rest) = line.trim().strip_prefix("#[path = \"") else { continue };
            let target = rest.split('"').next().expect("a path");
            let joined = file.parent().expect("a directory").join(target);
            let mut parts: Vec<String> = Vec::new();
            for part in joined.components() {
                match part {
                    std::path::Component::ParentDir => {
                        parts.pop();
                    }
                    std::path::Component::Normal(p) => parts.push(p.to_string_lossy().to_string()),
                    _ => {}
                }
            }
            let included = parts.join("/");
            assert!(listed.contains(&included), "{included}, which {name} compiles in, isn't among the image's sources: {listed:?}");
        }
    }
}
