//! `--check`'s disk warning (SME-229): the kubelet deletes unused images
//! once its image disk is 85% full (`imageGCHighThresholdPercent`), and the
//! images the gate needs are unused between runs, so a node near that line
//! loses them mid-run (SME-184, SME-185, SME-239). The check warns while
//! there's still room, naming what the kubelet could delete.
//!
//! Read from the loader pod, which mounts the containerd root (where k3s
//! keeps images, the kubelet's image filesystem) read-only: the `park`
//! account the gate runs as can't read the node's stats API, and
//! shouldn't need a cluster role for a warning. Nothing here fails the
//! check: a disk it can't read is a note, not an error.

use kube::api::Api;
use k8s_openapi::api::core::v1::Pod;

use super::{exec_capture, BoxError, CONTAINERD_SOCKET};

/// Where k3s keeps containerd's images on the node, the same layout
/// `CONTAINERD_SOCKET`'s `/run/k3s` assumes.
pub(crate) const IMAGE_FS_HOST_PATH: &str = "/var/lib/rancher/k3s/agent/containerd";
/// Where the loader pod mounts it.
pub(crate) const IMAGE_FS_MOUNT: &str = "/hostimagefs";

/// The kubelet's default `imageGCHighThresholdPercent`, which neither the
/// compose `k3s` service nor a CI runner changes.
const GC_HIGH_PERCENT: u64 = 85;
/// How close to that line the check starts warning (the ticket's "about
/// 15 GB": a few sandbox images and a gate's build cache).
const WARN_MARGIN_BYTES: u64 = 15 << 30;

/// The image filesystem's size and free space, in bytes.
#[derive(Debug, PartialEq)]
pub(crate) struct DiskUse {
    pub(crate) capacity: u64,
    pub(crate) available: u64,
}

impl DiskUse {
    /// The percent full the kubelet compares with its threshold:
    /// `100 - available * 100 / capacity`, as its image GC does.
    fn percent_full(&self) -> u64 {
        100 - (self.available.saturating_mul(100) / self.capacity.max(1)).min(100)
    }

    /// Bytes left before the kubelet starts deleting images (zero once past).
    fn headroom(&self) -> u64 {
        let reserve = self.capacity / 100 * (100 - GC_HIGH_PERCENT);
        self.available.saturating_sub(reserve)
    }
}

/// `df -Pk <path>`'s data line: `fs 1024-blocks used available capacity mount`.
pub(crate) fn parse_df(out: &str) -> Option<DiskUse> {
    out.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let capacity = fields.get(1)?.parse::<u64>().ok()?;
        let available = fields.get(3)?.parse::<u64>().ok()?;
        (capacity > 0).then(|| DiskUse { capacity: capacity * 1024, available: available * 1024 })
    })
}

/// An image on the node: its digest, its names, its size in bytes.
#[derive(Debug, PartialEq)]
pub(crate) struct NodeImage {
    pub(crate) digest: String,
    pub(crate) names: Vec<String>,
    pub(crate) size: u64,
}

impl NodeImage {
    /// Its tag name if it has one, else whatever it's listed as.
    fn display_name(&self) -> &str {
        self.names
            .iter()
            .find(|n| !n.starts_with("sha256:") && !n.contains('@'))
            .or(self.names.first())
            .map_or(self.digest.as_str(), String::as_str)
    }
}

/// `ctr images ls`'s rows (`REF TYPE DIGEST SIZE PLATFORMS LABELS`, the size
/// two words such as `1.2 GiB`), one entry per digest with every name.
pub(crate) fn parse_images(out: &str) -> Vec<NodeImage> {
    let mut images: Vec<NodeImage> = Vec::new();
    for line in out.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (Some(name), Some(digest), Some(number), Some(unit)) =
            (fields.first(), fields.get(2), fields.get(3), fields.get(4))
        else {
            continue;
        };
        let (Some(size), true) = (parse_size(number, unit), digest.starts_with("sha256:")) else {
            continue;
        };
        match images.iter_mut().find(|i| i.digest == *digest) {
            Some(image) => image.names.push((*name).to_string()),
            None => images.push(NodeImage {
                digest: (*digest).to_string(),
                names: vec![(*name).to_string()],
                size,
            }),
        }
    }
    images
}

/// `ctr`'s human sizes: `512 B`, `3.4 KiB`, `1.2 GiB`.
fn parse_size(number: &str, unit: &str) -> Option<u64> {
    let shift = match unit {
        "B" => 0,
        "KiB" => 10,
        "MiB" => 20,
        "GiB" => 30,
        "TiB" => 40,
        _ => return None,
    };
    let value = number.parse::<f64>().ok()?;
    (value.is_finite() && value >= 0.0).then(|| (value * (1u64 << shift) as f64) as u64)
}

/// The image names `ctr containers ls` (`CONTAINER IMAGE RUNTIME`) lists:
/// what the node's containers, running or stopped, were made from. The
/// kubelet deletes only images no container uses.
pub(crate) fn parse_container_images(out: &str) -> Vec<String> {
    out.lines()
        .skip_while(|line| !line.starts_with("CONTAINER"))
        .skip(1)
        .take_while(|line| !line.starts_with("[stderr]"))
        .filter_map(|line| line.split_whitespace().nth(1).map(str::to_string))
        .collect()
}

/// The warning, when `disk` is within `WARN_MARGIN_BYTES` of the threshold:
/// how full it is and the images no container uses, the ones in `wanted`
/// (the gate's) first, then the biggest.
pub(crate) fn warning(disk: &DiskUse, images: &[NodeImage], in_use: &[String], wanted: &[String]) -> Option<String> {
    let headroom = disk.headroom();
    if headroom >= WARN_MARGIN_BYTES {
        return None;
    }
    let mut unused: Vec<&NodeImage> = images
        .iter()
        .filter(|image| !image.names.iter().any(|n| in_use.contains(n)))
        .collect();
    let is_wanted = |image: &NodeImage| image.names.iter().any(|n| wanted.contains(n));
    unused.sort_by_key(|image| (!is_wanted(image), std::cmp::Reverse(image.size)));
    let listed: Vec<String> = unused
        .iter()
        .take(8)
        .map(|image| format!("{} ({})", image.display_name(), gib(image.size)))
        .collect();
    let more = unused.len().saturating_sub(listed.len());
    let line = if headroom == 0 { "is past" } else { "is near" };
    let mut text = format!(
        "warning: the node's image disk is {}% full, {} free: it {line} the kubelet's {GC_HIGH_PERCENT}% \
         image garbage-collection line ({} before it), past which it deletes unused images, the gate's \
         own included",
        disk.percent_full(),
        gib(disk.available),
        gib(headroom),
    );
    if listed.is_empty() {
        text.push_str("; no image is unused");
    } else {
        text.push_str(&format!("; unused now: {}", listed.join(", ")));
        if more > 0 {
            text.push_str(&format!(" and {more} more"));
        }
    }
    text.push_str(". Free space on the node (delete unused images, prune the Docker cache) before it does.");
    Some(text)
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / f64::from(1u32 << 30))
}

/// Reads the loader's view of the node and prints the warning if there is
/// one; any failure to read is a note on stderr, never the check's failure.
pub(crate) async fn warn_if_near_gc(pods: &Api<Pod>, loader: &str, wanted: &[String]) {
    if let Err(e) = try_warn(pods, loader, wanted).await {
        eprintln!("sandbox_image_import: couldn't read the node's image disk ({e}); no disk warning");
    }
}

async fn try_warn(pods: &Api<Pod>, loader: &str, wanted: &[String]) -> Result<(), BoxError> {
    let df = exec_capture(pods, loader, &["df", "-Pk", IMAGE_FS_MOUNT]).await?;
    let disk = parse_df(&df).ok_or_else(|| format!("df printed {:?}", df.trim()))?;
    let ctr = |args: &'static [&'static str]| {
        let mut command = vec!["ctr", "--address", CONTAINERD_SOCKET, "--namespace", "k8s.io"];
        command.extend_from_slice(args);
        command
    };
    if disk.headroom() >= WARN_MARGIN_BYTES {
        return Ok(());
    }
    let images = parse_images(&exec_capture(pods, loader, &ctr(&["images", "ls"])).await?);
    let in_use = parse_container_images(&exec_capture(pods, loader, &ctr(&["containers", "ls"])).await?);
    if let Some(text) = warning(&disk, &images, &in_use, wanted) {
        eprintln!("{text}");
    }
    Ok(())
}

#[cfg(test)]
#[path = "disk/tests.rs"]
mod tests;
