//! Every network fetch the build and CI make goes through a guard (SME-470).
//!
//! A `curl` or `wget` of a non-local URL in a Dockerfile, a workflow or a
//! script is a fetch with no retry, no bound or no pin unless it goes
//! through `scripts/fetch-pinned` (or `scripts/ci-install-compose`, which
//! has its own sources). One that can't, such as the sandbox image's, whose
//! build context has no `scripts/`, says why in a `# fetch: <reason>`
//! comment, on the command's last line or the comment lines right above it.
//! A fetch piped into a shell (`| sudo -E bash`, `| /bin/sh` too), or handed
//! to one as `bash <(curl …)` or `sh -c "$(curl …)"`, fails whatever its
//! comment says: the shell runs a body cut off partway as readily as a
//! whole one.

/// The files the build and CI run: Dockerfiles and the images' shell
/// scripts, the workflows and their actions, and the scripts, `scripts/lib/`
/// included (review round 1, N1), except the scripts' own test suites
/// (their fetches are of a stub server or a probe).
pub(crate) fn in_scope(path: &str) -> bool {
    if EXEMPT.contains(&path) {
        return false;
    }
    path == "Dockerfile"
        || (path.starts_with("docker/") && (path.ends_with("/Dockerfile") || path.ends_with(".sh")))
        || (path.starts_with(".github/") && (path.ends_with(".yml") || path.ends_with(".yaml")))
        || (path.starts_with("scripts/") && !path.starts_with("scripts/test-"))
}

/// The guards themselves.
const EXEMPT: &[&str] = &["scripts/fetch-pinned", "scripts/ci-install-compose"];

/// One `path:line: why: text` per unguarded fetch in `text`.
pub(crate) fn unguarded_fetches(path: &str, text: &str) -> Vec<String> {
    let fetch = regex::Regex::new(r#"(?:^|[\s;&|(!])(?:curl|wget)\s+(?:-|["']|https?://|\$)"#).expect("a valid regex");
    // A shell by name or path (`bash`, `/usr/bin/sh`, `zsh`), reading a
    // fetch from a pipe (through `sudo [-E …]` or `env` too), from `<(…)`,
    // or running it from `-c "$(…)"`.
    let shell = r"(?:\S*/)?(?:ba|z|da|k)?sh\b";
    let piped = regex::Regex::new(&format!(
        r#"\b(?:curl|wget)\b[^#]*\|\s*(?:sudo(?:\s+-\S+)*\s+)?(?:env\s+)?{shell}|(?:^|[\s;&|(!]){shell}\s+(?:<\(|(?:-\S+\s+)*["']?\$\()\s*(?:curl|wget)\b"#
    ))
    .expect("a valid regex");
    let url = regex::Regex::new(r"https?://(\[[^\]]*\]|[^/\s'`$:]+)").expect("a valid regex");
    let mut findings = Vec::new();
    let mut comments_above = String::new();
    let mut logical = String::new();
    let mut start = 0;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if logical.is_empty() && line.starts_with('#') {
            comments_above.push_str(line);
            comments_above.push('\n');
            continue;
        }
        if logical.is_empty() {
            start = i + 1;
        }
        match line.strip_suffix('\\') {
            Some(head) => {
                logical.push_str(head);
                logical.push(' ');
                continue;
            }
            None => logical.push_str(line),
        }
        let excused = has_reason(&logical) || comments_above.lines().any(has_reason);
        let shown: String = logical.chars().take(120).collect();
        if piped.is_match(&logical) {
            findings.push(format!("{path}:{start}: pipes a fetch into a shell: {shown}"));
        } else if fetch.is_match(&logical) && !excused {
            let hosts: Vec<&str> = url.captures_iter(&logical).map(|c| c.get(1).map_or("", |m| m.as_str())).collect();
            let local = !hosts.is_empty() && hosts.iter().all(|h| matches!(*h, "localhost" | "127.0.0.1" | "[::1]"));
            if !local {
                findings.push(format!(
                    "{path}:{start}: fetches with no guard (use scripts/fetch-pinned, or add `# fetch: <reason>`): {shown}"
                ));
            }
        }
        logical.clear();
        comments_above.clear();
    }
    findings
}

/// A `# fetch: <reason>` comment with a reason in it.
fn has_reason(text: &str) -> bool {
    text.split_once("# fetch:").is_some_and(|(_, reason)| !reason.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(text: &str) -> Vec<String> {
        unguarded_fetches("Dockerfile", text)
    }

    #[test]
    fn test_a_bare_curl_of_a_url_is_found() {
        let f = found("RUN curl -fsSL https://example.com/x.tgz -o x.tgz\n");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].starts_with("Dockerfile:1: fetches with no guard"), "{f:?}");
    }

    #[test]
    fn test_a_fetch_piped_into_a_shell_is_found_even_with_a_reason() {
        let f = found("RUN curl -fsSL https://claude.ai/install.sh | bash # fetch: because\n");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("pipes a fetch into a shell"), "{f:?}");
        assert_eq!(found("RUN curl -sSf https://x.example/i.sh \\\n    | sudo sh\n").len(), 1);
    }

    fn piped_even_with_a_reason(line: &str) {
        let f = found(&format!("{line} # fetch: because\n"));
        assert_eq!(f.len(), 1, "{line}: {f:?}");
        assert!(f[0].contains("pipes a fetch into a shell"), "{line}: {f:?}");
    }

    #[test]
    fn test_a_fetch_piped_into_sudo_with_flags_is_found() {
        piped_even_with_a_reason("curl -fsSL https://x.example/i.sh | sudo -E bash");
        piped_even_with_a_reason("wget -qO- https://x.example/i.sh | sudo -E -H sh");
        piped_even_with_a_reason("curl -fsSL https://x.example/i.sh | sudo bash -s -- --yes");
    }

    #[test]
    fn test_a_fetch_piped_into_a_shell_by_path_is_found() {
        piped_even_with_a_reason("curl -fsSL https://x.example/i.sh | /bin/bash");
        piped_even_with_a_reason("curl -fsSL https://x.example/i.sh | /usr/bin/sh");
        piped_even_with_a_reason("curl -fsSL https://x.example/i.sh | sudo /usr/local/bin/bash");
        piped_even_with_a_reason("curl -fsSL https://x.example/i.sh | env bash");
        piped_even_with_a_reason("curl -fsSL https://x.example/i.sh | zsh");
        assert_eq!(found("curl -fsSL https://x.example/a | sha256sum # fetch: a probe\n").len(), 0, "sha256sum isn't a shell");
    }

    #[test]
    fn test_a_shell_reading_a_fetch_by_process_substitution_is_found() {
        piped_even_with_a_reason("bash <(curl -fsSL https://x.example/i.sh)");
        piped_even_with_a_reason("sh <( wget -qO- https://x.example/i.sh )");
    }

    #[test]
    fn test_a_shell_running_a_fetch_by_command_substitution_is_found() {
        piped_even_with_a_reason(r#"sh -c "$(curl -fsSL https://x.example/i.sh)""#);
        piped_even_with_a_reason(r#"bash -c "$(wget -qO- https://x.example/i.sh)""#);
        piped_even_with_a_reason(r#"/bin/bash -ec "$(curl -fsSL https://x.example/i.sh)""#);
    }

    #[test]
    fn test_a_continued_command_is_one_finding_at_its_first_line() {
        let f = found("FROM x\nRUN set -eu; \\\n    curl -fsSL \\\n        \"https://example.com/a\" -o a\n");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].starts_with("Dockerfile:2:"), "{f:?}");
    }

    #[test]
    fn test_a_fetch_reason_passes_trailing_or_above() {
        assert!(found("curl -fsSL https://example.com/a -o a # fetch: probed only\n").is_empty());
        let above = "# The sandbox image's context has no scripts/.\n# fetch: no fetch-pinned here; same guard inline\nRUN curl --retry 4 \\\n    https://example.com/a -o a\n";
        assert!(found(above).is_empty(), "{:?}", found(above));
        assert_eq!(found("curl -fsSL https://example.com/a -o a # fetch:\n").len(), 1, "an empty reason");
    }

    #[test]
    fn test_a_reason_covers_only_the_next_command() {
        let text = "# fetch: the first one only\ncurl -fsSL https://example.com/a -o a\ncurl -fsSL https://example.com/b -o b\n";
        let f = found(text);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].starts_with("Dockerfile:3:"), "{f:?}");
    }

    #[test]
    fn test_local_urls_and_package_names_are_not_fetches() {
        assert!(found("curl -4 -sf -o /dev/null --max-time 60 \"http://localhost:$port/\"\n").is_empty());
        assert!(found("curl -sf http://127.0.0.1:8080/health\n").is_empty());
        assert!(found("RUN apt-get install -y \\\n    curl \\\n    git\n").is_empty());
        assert!(found("# curl -fsSL https://example.com is how it used to be\n").is_empty());
        assert_eq!(found("curl -fsSL http://localhost/a https://example.com/b\n").len(), 1, "one remote URL");
    }

    #[test]
    fn test_a_url_in_a_variable_is_a_fetch() {
        assert_eq!(found("    curl -fsSLO \"$repo/$V/manifest.json\"; \\\n    true\n").len(), 1);
        assert_eq!(found("xargs -P 4 -n 1 curl -fsSL --retry 3 -O\n").len(), 1);
    }

    #[test]
    fn test_scope_is_the_build_and_ci_files() {
        for p in ["Dockerfile", "docker/sandbox/Dockerfile", ".github/workflows/ci.yml", ".github/actions/start-stack/action.yml", "scripts/browser-check/setup.sh", "scripts/check-server"] {
            assert!(in_scope(p), "{p} is in scope");
        }
        // Review round 1, N1: check-server's sourced libraries, their test
        // helpers, and the shell scripts the images run.
        for p in ["scripts/lib/check-server-url.sh", "scripts/lib/dotenv-value", "scripts/lib/test-check-scripts.sh", "scripts/lib/flaky-http-server.py", "docker/server-bundle.sh", "docker/sandbox/start-dockerd.sh"] {
            assert!(in_scope(p), "{p} is in scope");
        }
        for p in ["scripts/fetch-pinned", "scripts/ci-install-compose", "scripts/test-fetch-pinned", "src/lsp/pods/script.rs", "docs/setup.md", "docker/sandbox/README.md"] {
            assert!(!in_scope(p), "{p} is out of scope");
        }
    }

    /// The sweep stays swept (SME-470): no Dockerfile, workflow or script
    /// fetches past the guards.
    #[test]
    fn test_no_build_or_ci_fetch_is_unguarded() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let files: Vec<String> = super::super::tracked_files().into_iter().filter(|p| in_scope(p)).collect();
        assert!(files.iter().any(|p| p == "Dockerfile"), "the walk found the Dockerfile: {files:?}");
        let findings: Vec<String> = files
            .iter()
            .filter_map(|p| Some((p, std::fs::read_to_string(root.join(p)).ok()?)))
            .flat_map(|(p, text)| unguarded_fetches(p, &text))
            .collect();
        assert!(
            findings.is_empty(),
            "unguarded fetches (SME-470; docs/setup.md, \"Fetches at build time\"):\n{}",
            findings.join("\n")
        );
    }

    /// The Dockerfile's dx is the one the `dioxus` crate needs: a mismatched
    /// dx refuses to build the project (SME-470, which pins it by DX_VERSION
    /// and its tarball's sha256 in place of cargo-binstall).
    #[test]
    fn test_dockerfile_pins_cargo_locks_dioxus() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let lock = std::fs::read_to_string(root.join("Cargo.lock")).expect("read Cargo.lock");
        let locked: Vec<&str> = lock
            .split("[[package]]")
            .filter(|p| p.lines().any(|l| l.trim() == r#"name = "dioxus""#))
            .filter_map(|p| p.lines().find_map(|l| l.trim().strip_prefix("version = ")))
            .map(|v| v.trim_matches('"'))
            .collect();
        assert_eq!(locked.len(), 1, "Cargo.lock should lock exactly one dioxus: {locked:?}");
        let dockerfile = std::fs::read_to_string(root.join("Dockerfile")).expect("read the Dockerfile");
        let pins: Vec<&str> = dockerfile.lines().filter_map(|l| l.trim().strip_prefix("ARG DX_VERSION=")).collect();
        assert_eq!(pins.len(), 1, "exactly one ARG DX_VERSION= line in Dockerfile: {pins:?}");
        assert_eq!(
            pins[0], locked[0],
            "the Dockerfile pins dx {} but Cargo.lock locks dioxus {}: bump DX_VERSION and its sha256s",
            pins[0], locked[0]
        );
    }
}
