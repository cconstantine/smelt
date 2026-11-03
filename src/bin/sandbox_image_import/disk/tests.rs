use super::*;

const GIB: u64 = 1 << 30;

fn disk(capacity_gib: u64, available_gib: u64) -> DiskUse {
    DiskUse { capacity: capacity_gib * GIB, available: available_gib * GIB }
}

fn image(digest: &str, names: &[&str], size_gib: u64) -> NodeImage {
    NodeImage {
        digest: digest.to_string(),
        names: names.iter().map(|n| n.to_string()).collect(),
        size: size_gib * GIB,
    }
}

#[test]
fn test_df_gives_the_image_disks_size_and_free_space() {
    let out = "Filesystem     1024-blocks      Used Available Capacity Mounted on\n\
               /dev/nvme0n1p2   488281250 390625000  97656250      80% /hostimagefs\n";
    assert_eq!(parse_df(out), Some(DiskUse { capacity: 488281250 * 1024, available: 97656250 * 1024 }));
    assert_eq!(parse_df("df: /hostimagefs: No such file or directory\n"), None);
}

/// The kubelet's threshold is 85% full: a 500 GiB disk has 75 GiB it keeps
/// free, so 100 GiB free is 25 GiB from the line, 80 GiB free is 5 from it.
#[test]
fn test_the_warning_starts_within_15_gib_of_the_kubelets_threshold() {
    assert_eq!(warning(&disk(500, 100), &[], &[], &[]), None);
    assert_eq!(warning(&disk(500, 91), &[], &[], &[]), None);
    let near = warning(&disk(500, 80), &[], &[], &[]).unwrap_or_default();
    assert!(near.contains("84% full"), "{near}");
    assert!(near.contains("is near"), "{near}");
    assert!(near.contains("5.0 GiB before it"), "{near}");
    let past = warning(&disk(500, 60), &[], &[], &[]).unwrap_or_default();
    assert!(past.contains("is past"), "{past}");
    assert!(past.contains("0.0 GiB before it"), "{past}");
}

/// What the kubelet could delete: images no container uses, the gate's own
/// first (what the next run needs), then the biggest.
#[test]
fn test_the_warning_names_unused_images_the_gates_first() {
    let sandbox = "docker.io/library/smelt-sandbox:src-0123456789abcdef";
    let images = [
        image("sha256:aa", &["docker.io/library/old:1"], 2),
        image("sha256:bb", &["docker.io/library/running:1"], 9),
        image("sha256:cc", &[sandbox, "docker.io/library/smelt-sandbox@sha256:cc"], 1),
        image("sha256:dd", &["docker.io/library/big:1"], 5),
    ];
    let in_use = vec!["docker.io/library/running:1".to_string()];
    let text = warning(&disk(500, 80), &images, &in_use, &[sandbox.to_string()]).unwrap_or_default();
    let unused = text.split("unused now: ").nth(1).unwrap_or_default();
    assert!(
        unused.starts_with(&format!("{sandbox} (1.0 GiB), docker.io/library/big:1 (5.0 GiB), docker.io/library/old:1 (2.0 GiB).")),
        "{text}"
    );
    assert!(!text.contains("running:1"), "{text}");
}

#[test]
fn test_ctr_listings_parse_into_images_and_what_containers_use() {
    let images = "REF TYPE DIGEST SIZE PLATFORMS LABELS\n\
        docker.io/library/docker:29-dind application/vnd.oci.image.index.v1+json sha256:d1 120.5 MiB linux/amd64 -\n\
        docker.io/library/docker@sha256:d1 application/vnd.oci.image.index.v1+json sha256:d1 120.5 MiB linux/amd64 -\n\
        sha256:9f00 application/vnd.oci.image.manifest.v1+json sha256:e2 1.5 GiB linux/amd64 io.cri-containerd.image=managed\n";
    let parsed = parse_images(images);
    assert_eq!(parsed.len(), 2, "{parsed:?}");
    assert_eq!(parsed[0].names.len(), 2);
    assert_eq!(parsed[0].size, (120.5 * f64::from(1u32 << 20)) as u64);
    assert_eq!(parsed[1].display_name(), "sha256:9f00");
    assert_eq!(parsed[1].size, 3 * GIB / 2);

    let containers = "CONTAINER    IMAGE                                   RUNTIME\n\
        0a1b         docker.io/rancher/mirrored-pause:3.6    io.containerd.runc.v2\n\
        2c3d         docker.io/library/smelt-sandbox:src-ab  io.containerd.runc.v2\n";
    assert_eq!(
        parse_container_images(containers),
        vec!["docker.io/rancher/mirrored-pause:3.6".to_string(), "docker.io/library/smelt-sandbox:src-ab".to_string()]
    );
}
