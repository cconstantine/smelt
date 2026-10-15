//! Checks on the repo's own files, run with the tests (SME-163).
//!
//! The repo is read by agents with limited context, the local models'
//! 262k-token window among them, so every tracked text file stays under a
//! ceiling: [`CEILING`], about 11k tokens, one read in Claude Code and
//! opencode. See docs/process/rules-commits.md, "Files stay under the
//! ceiling".

mod components;
mod links;

/// The most a tracked text file may hold.
pub(crate) const CEILING: Limit = Limit {
    bytes: 40_000,
    lines: 2_000,
};

/// Files that aren't held to the ceiling: generated ones.
const NOT_COUNTED: &[&str] = &["Cargo.lock", "assets/highlight.css"];

/// Files over the ceiling that haven't been split yet, each held to its size
/// when it was listed. The list only shrinks: a listed file that grows fails
/// the check, and so does one that is now under the ceiling (or gone), until
/// its entry is removed.
const EXCEPTIONS: &[(&str, Limit)] = &[
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limit {
    pub(crate) bytes: u64,
    pub(crate) lines: u64,
}

/// One tracked file's size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Measured {
    pub(crate) path: String,
    pub(crate) bytes: u64,
    pub(crate) lines: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Finding {
    /// Over the ceiling, and not listed.
    Over(Measured),
    /// Listed, and grown past the size it was listed at.
    Grew { file: Measured, cap: Limit },
    /// Listed, but now under the ceiling, or gone: remove its entry.
    Stale(String),
}

/// Every file over the ceiling that isn't excused, every listed file over its
/// listed size, and every entry that no longer excuses anything.
pub(crate) fn oversized(files: &[Measured], ceiling: Limit, exceptions: &[(&str, Limit)]) -> Vec<Finding> {
    let over = |f: &Measured, limit: Limit| f.bytes > limit.bytes || f.lines > limit.lines;
    let mut findings = Vec::new();
    for f in files {
        match exceptions.iter().find(|(path, _)| *path == f.path) {
            None if over(f, ceiling) => findings.push(Finding::Over(f.clone())),
            Some((_, cap)) if over(f, *cap) => findings.push(Finding::Grew { file: f.clone(), cap: *cap }),
            _ => {}
        }
    }
    for (path, _) in exceptions {
        let still_over = files.iter().any(|f| f.path == *path && over(f, ceiling));
        if !still_over {
            findings.push(Finding::Stale(path.to_string()));
        }
    }
    findings
}

/// Every file git tracks under the repo root, relative to it.
fn tracked_files() -> Vec<String> {
    let root = env!("CARGO_MANIFEST_DIR");
    // `safe.directory`: CI's container runs as another user than the one
    // that checked the repo out.
    let out = std::process::Command::new("git")
        .args(["-c", "safe.directory=*", "-C", root, "ls-files", "-z"])
        .output()
        .expect("run git ls-files");
    assert!(out.status.success(), "git ls-files failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout)
        .expect("tracked paths are UTF-8")
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// The tracked text files, measured. Binary files (a NUL byte), generated
/// ones and files deleted from the working tree but not from the index are
/// left out.
fn measured_text_files() -> Vec<Measured> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for path in tracked_files() {
        if NOT_COUNTED.contains(&path.as_str()) || path.ends_with(".terraform.lock.hcl") {
            continue;
        }
        let Ok(bytes) = std::fs::read(root.join(&path)) else { continue };
        if bytes.contains(&0) {
            continue;
        }
        let lines = bytes.iter().filter(|b| **b == b'\n').count() as u64;
        files.push(Measured {
            path,
            bytes: bytes.len() as u64,
            lines,
        });
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_docs_links_resolve() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let tracked: std::collections::HashSet<String> = tracked_files().into_iter().collect();
        let docs: std::collections::BTreeMap<String, String> = tracked
            .iter()
            .filter(|p| p.ends_with(".md"))
            .filter_map(|p| Some((p.clone(), std::fs::read_to_string(root.join(p)).ok()?)))
            .collect();
        assert!(docs.contains_key("README.md"), "the walk found the docs");
        let exists = |p: &str| tracked.contains(p) || tracked.iter().any(|t| t.starts_with(&format!("{p}/")));
        let broken = links::broken_links(&docs, &exists);
        assert!(broken.is_empty(), "links that don't resolve:\n{}", broken.join("\n"));
    }

    /// The app links every part of its stylesheet, in order, and the browser
    /// tier serves each (SME-163): a part left out would leave pages partly
    /// unstyled, and parts out of order would change which rule wins a tie.
    #[test]
    fn test_every_stylesheet_part_is_linked_in_order() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut parts: Vec<String> = tracked_files()
            .into_iter()
            .filter(|p| p.starts_with("assets/css/") && p.ends_with(".css"))
            .collect();
        parts.sort();
        assert!(parts.len() > 1, "the stylesheet's parts: {parts:?}");
        let links = regex::Regex::new(r#"asset!\("/(assets/css/[^"]+)"\)"#).expect("a valid regex");
        for file in ["src/frontend/mod.rs", "src/browser_tests/harness.rs"] {
            let text = std::fs::read_to_string(root.join(file)).expect("read the file that links the stylesheet");
            let linked: Vec<String> = links.captures_iter(&text).map(|c| c[1].to_string()).collect();
            assert_eq!(linked, parts, "{file} should link every part of assets/css/, in order");
        }
    }

    /// clippy can't see inside a `#[component]`, so components are held to
    /// its 200-code-line limit here (SME-163).
    #[test]
    fn test_no_component_is_over_the_function_length_limit() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut over = Vec::new();
        let mut seen = 0;
        for path in tracked_files().into_iter().filter(|p| p.starts_with("src/") && p.ends_with(".rs")) {
            let Ok(source) = std::fs::read_to_string(root.join(&path)) else { continue };
            for (name, lines) in components::component_lengths(&source) {
                seen += 1;
                if lines > components::MAX_COMPONENT_LINES {
                    over.push(format!("{path}: {name} is {lines} code lines"));
                }
            }
        }
        assert!(seen > 20, "found the components ({seen})");
        assert!(
            over.is_empty(),
            "components over {} code lines: split each into child components along its own parts\n{}",
            components::MAX_COMPONENT_LINES,
            over.join("\n")
        );
    }

    #[test]
    fn test_no_file_crosses_the_size_ceiling() {
        let files = measured_text_files();
        assert!(files.iter().any(|f| f.path == "src/main.rs"), "the walk found the repo's files");
        let findings = oversized(&files, CEILING, EXCEPTIONS);
        let report: Vec<String> = findings
            .iter()
            .map(|f| match f {
                Finding::Over(m) => format!(
                    "{} is {} bytes and {} lines, over the ceiling of {} and {}: split it",
                    m.path, m.bytes, m.lines, CEILING.bytes, CEILING.lines
                ),
                Finding::Grew { file, cap } => format!(
                    "{} grew to {} bytes and {} lines, past the {} and {} it's listed at: split it rather than raising its entry",
                    file.path, file.bytes, file.lines, cap.bytes, cap.lines
                ),
                Finding::Stale(path) => format!("{path} is under the ceiling now (or gone): remove its EXCEPTIONS entry"),
            })
            .collect();
        assert!(
            report.is_empty(),
            "files over the size ceiling (see docs/process/rules-commits.md, \"Files stay under the ceiling\"):\n{}",
            report.join("\n")
        );
    }

    fn file(path: &str, bytes: u64, lines: u64) -> Measured {
        Measured {
            path: path.to_string(),
            bytes,
            lines,
        }
    }

    const SMALL: Limit = Limit { bytes: 100, lines: 10 };

    #[test]
    fn test_a_file_under_the_ceiling_is_fine() {
        assert_eq!(oversized(&[file("a.rs", 100, 10)], SMALL, &[]), vec![]);
    }

    #[test]
    fn test_a_file_over_the_byte_ceiling_is_reported() {
        let big = file("a.rs", 101, 3);
        assert_eq!(oversized(&[big.clone()], SMALL, &[]), vec![Finding::Over(big)]);
    }

    #[test]
    fn test_a_file_over_the_line_ceiling_is_reported() {
        let long = file("a.css", 50, 11);
        assert_eq!(oversized(&[long.clone()], SMALL, &[]), vec![Finding::Over(long)]);
    }

    #[test]
    fn test_a_listed_file_may_stay_at_its_listed_size() {
        let cap = Limit { bytes: 500, lines: 40 };
        let files = [file("big.rs", 500, 40), file("small.rs", 10, 1)];
        assert_eq!(oversized(&files, SMALL, &[("big.rs", cap)]), vec![]);
    }

    #[test]
    fn test_a_listed_file_that_grows_is_reported() {
        let cap = Limit { bytes: 500, lines: 40 };
        let grown = file("big.rs", 501, 40);
        assert_eq!(
            oversized(&[grown.clone()], SMALL, &[("big.rs", cap)]),
            vec![Finding::Grew { file: grown, cap }]
        );
        let longer = file("big.rs", 400, 41);
        assert_eq!(
            oversized(&[longer.clone()], SMALL, &[("big.rs", cap)]),
            vec![Finding::Grew { file: longer, cap }]
        );
    }

    #[test]
    fn test_a_listed_file_now_under_the_ceiling_is_stale() {
        let cap = Limit { bytes: 500, lines: 40 };
        assert_eq!(
            oversized(&[file("big.rs", 90, 9)], SMALL, &[("big.rs", cap)]),
            vec![Finding::Stale("big.rs".to_string())]
        );
    }

    #[test]
    fn test_a_listed_file_that_is_gone_is_stale() {
        let cap = Limit { bytes: 500, lines: 40 };
        assert_eq!(
            oversized(&[file("other.rs", 9, 1)], SMALL, &[("big.rs", cap)]),
            vec![Finding::Stale("big.rs".to_string())]
        );
    }
}
