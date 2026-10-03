//! The streaming reply and the working line.

/// How long a turn has been running, as shown on its "Working…" line:
/// `8s`, `2m 05s`.
pub(super) fn format_elapsed(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}
