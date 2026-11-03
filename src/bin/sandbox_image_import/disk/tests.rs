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
        pinned: false,
    }
}

#[test]
fn test_df_gives_the_image_disks_size_and_free_space() {
    let out = "Filesystem     1024-blocks      Used Available Capacity Mounted on\n\
               /dev/nvme0n1p2   488281250 390625000  97656250      80% /hostimagefs\n";
    assert_eq!(parse_df(out), Some(DiskUse { capacity: 488281250 * 1024, available: 97656250 * 1024 }));
    assert_eq!(parse_df("df: /hostimagefs: No such file or directory\n"), None);
}

/// The kubelet's percent is an integer, `100 - floor(free * 100 / size)`,
/// so it collects at 85% as soon as less than 16% is free: on a 500 GiB
/// disk below 80 GiB free, so the warning starts below 95 (SME-229 review).
#[test]
fn test_the_warning_starts_within_15_gib_of_the_kubelets_threshold() {
    assert_eq!(warning(&disk(500, 100), &[], &[], &[]), None);
    assert_eq!(warning(&disk(500, 95), &[], &[], &[]), None);
    let near = warning(&disk(500, 94), &[], &[], &[]).unwrap_or_default();
    assert!(near.contains("82% full"), "{near}");
    assert!(near.contains("is near"), "{near}");
    assert!(near.contains("14.0 GiB before it"), "{near}");
    let edge = warning(&disk(500, 80), &[], &[], &[]).unwrap_or_default();
    assert!(edge.contains("84% full") && edge.contains("is near"), "{edge}");
    let past = warning(&disk(500, 79), &[], &[], &[]).unwrap_or_default();
    assert!(past.contains("85% full") && past.contains("is past"), "{past}");
    assert!(past.contains("0.0 GiB before it"), "{past}");
}

/// On a big disk the missing 1% is more than the margin: this host's
/// 1832 GiB collects below 293.2 GiB free, so 300 free must warn.
#[test]
fn test_a_big_disk_warns_before_the_kubelet_collects() {
    let text = warning(&disk(1832, 300), &[], &[], &[]).unwrap_or_default();
    assert!(text.contains("is near"), "{text}");
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
    let text = warning(&disk(500, 90), &images, &in_use, &[sandbox.to_string()]).unwrap_or_default();
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

/// CRI-pinned images (the pause image, k3s's own) are never collected, so
/// they aren't named.
#[test]
fn test_pinned_images_arent_named_as_at_risk() {
    let listed = "REF TYPE DIGEST SIZE PLATFORMS LABELS\n\
        docker.io/rancher/mirrored-pause:3.6 application/vnd.oci.image.index.v1+json sha256:p1 300.0 KiB linux/amd64 io.cri-containerd.image=managed,io.cri-containerd.pinned=pinned\n\
        docker.io/library/old:1 application/vnd.oci.image.index.v1+json sha256:o1 1.0 GiB linux/amd64 io.cri-containerd.image=managed\n";
    let images = parse_images(listed);
    assert!(images[0].pinned && !images[1].pinned, "{images:?}");
    let text = warning(&disk(500, 90), &images, &[], &[]).unwrap_or_default();
    assert!(text.contains("old:1") && !text.contains("pause"), "{text}");
}

#[test]
fn test_df_values_too_big_to_be_bytes_are_unreadable_not_a_panic() {
    let out = format!("fs {} 0 {} 0% /hostimagefs\n", u64::MAX, u64::MAX);
    assert_eq!(parse_df(&out), None);
}
