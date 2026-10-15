//! Every relative link in the repo's Markdown, and every `#anchor`, resolves
//! (SME-163). The docs are split across many files, and a section that moves
//! takes its anchor with it: this finds each link left pointing at the old
//! place. Links to this repo on GitHub (`https://github.com/cconstantine/smelt/blob/main/…`)
//! are checked against the working tree the same way.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

/// A link to this repo's own files on GitHub, checked like a relative one.
const REPO_URL: &str = "https://github.com/cconstantine/smelt/blob/main/";

/// GitHub's anchor for a heading: the rendered text, lowercased, with
/// punctuation other than `-` and `_` dropped and each space made a `-`.
pub(crate) fn slug(heading: &str) -> String {
    static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"!?\[([^\]]*)\]\([^)]*\)").expect("a valid regex"));
    static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").expect("a valid regex"));
    let text = LINK.replace_all(heading.trim(), "$1");
    let text = TAG.replace_all(&text, "");
    text.to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            '-' | '_' => Some(c),
            c if c.is_alphanumeric() => Some(c),
            _ => None,
        })
        .collect()
}

/// The lines of a Markdown file outside fenced code blocks, numbered from 1.
fn prose_lines(markdown: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut fence: Option<&str> = None;
    markdown.lines().enumerate().filter_map(move |(i, line)| {
        let t = line.trim_start();
        let marker = if t.starts_with("```") {
            Some("```")
        } else if t.starts_with("~~~") {
            Some("~~~")
        } else {
            None
        };
        match (fence, marker) {
            (None, Some(m)) => {
                fence = Some(m);
                None
            }
            (Some(f), Some(m)) if f == m => {
                fence = None;
                None
            }
            (Some(_), _) => None,
            (None, None) => Some((i + 1, line)),
        }
    })
}

/// Every anchor a Markdown file defines: its headings' slugs (a repeated one
/// gets `-1`, `-2`, …) and any `<a id="…">` or `<a name="…">`.
pub(crate) fn anchors(markdown: &str) -> HashSet<String> {
    static HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#{1,6}\s+(.+?)\s*#*\s*$").expect("a valid regex"));
    static HTML: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<a\s+(?:id|name)="([^"]+)""#).expect("a valid regex"));
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut found = HashSet::new();
    for (_, line) in prose_lines(markdown) {
        if let Some(c) = HEADING.captures(line) {
            let base = slug(&c[1]);
            let n = seen.entry(base.clone()).or_insert(0);
            found.insert(if *n == 0 { base.clone() } else { format!("{base}-{n}") });
            *n += 1;
        }
        for c in HTML.captures_iter(line) {
            found.insert(c[1].to_string());
        }
    }
    found
}

/// Each link target in a Markdown file, with its line number: inline links
/// and images, and reference definitions. Code blocks and code spans are
/// skipped.
pub(crate) fn links(markdown: &str) -> Vec<(usize, String)> {
    static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(`+)[^`]*?(`+)").expect("a valid regex"));
    static INLINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\]\(\s*(<[^>]*>|[^)\s]*)").expect("a valid regex"));
    static REFERENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\[[^\]]+\]:\s*(<[^>]*>|\S+)").expect("a valid regex"));
    let mut found = Vec::new();
    for (n, line) in prose_lines(markdown) {
        let line = CODE.replace_all(line, "");
        for c in INLINE.captures_iter(&line).chain(REFERENCE.captures_iter(&line)) {
            let target = c[1].trim_start_matches('<').trim_end_matches('>');
            if !target.is_empty() {
                found.push((n, target.to_string()));
            }
        }
    }
    found
}

/// `base`/`rel` with `.` and `..` resolved; `/rel` is from the repo root.
fn join(base: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = if rel.starts_with('/') { Vec::new() } else { base.split('/').filter(|p| !p.is_empty()).collect() };
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

/// Every link in `files` (path → Markdown) that doesn't resolve: a relative
/// path not in `exists`, or an anchor that the Markdown file it points at
/// doesn't define. Paths are relative to the repo root.
pub(crate) fn broken_links(files: &BTreeMap<String, String>, exists: &dyn Fn(&str) -> bool) -> Vec<String> {
    static SCHEME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([a-zA-Z][a-zA-Z0-9+.-]*:|//)").expect("a valid regex"));
    let mut anchor_cache: HashMap<&str, HashSet<String>> = HashMap::new();
    let mut broken = Vec::new();
    for (path, markdown) in files {
        let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
        for (line, target) in links(markdown) {
            let (base, rel) = match target.strip_prefix(REPO_URL) {
                Some(rest) => ("", rest),
                None if SCHEME.is_match(&target) => continue,
                None => (dir, target.as_str()),
            };
            let (file, anchor) = rel.split_once('#').map_or((rel, None), |(f, a)| (f, Some(a)));
            let resolved = if file.is_empty() { path.clone() } else { join(base, &file.replace("%20", " ")) };
            if !file.is_empty() && !exists(&resolved) {
                broken.push(format!("{path}:{line}: {resolved} doesn't exist"));
                continue;
            }
            let (Some(anchor), Some((key, target_md))) = (anchor, files.get_key_value(&resolved)) else { continue };
            let defined = anchor_cache.entry(key.as_str()).or_insert_with(|| anchors(target_md));
            if !defined.contains(anchor) {
                broken.push(format!("{path}:{line}: {resolved} has no anchor #{anchor}"));
            }
        }
    }
    broken
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slug_follows_github() {
        assert_eq!(slug("Rules every persona follows"), "rules-every-persona-follows");
        assert_eq!(slug("Phase 2: Implementation (TDD)"), "phase-2-implementation-tdd");
        assert_eq!(slug("Step 1 — Write a failing test"), "step-1--write-a-failing-test");
        assert_eq!(slug("`src/browser_tests.rs` (automated)"), "srcbrowser_testsrs-automated");
        assert_eq!(slug("Playwright (preferred)"), "playwright-preferred");
        assert_eq!(slug("[Linked](other.md) heading"), "linked-heading");
        assert_eq!(slug("What's not covered yet"), "whats-not-covered-yet");
    }

    #[test]
    fn test_anchors_number_repeated_headings_and_skip_code_blocks() {
        let md = "# Top\n\n## Notes\n\n```bash\n# not a heading\n```\n\n## Notes\n\n<a id=\"kept-anchor\"></a>\n";
        let got = anchors(md);
        for want in ["top", "notes", "notes-1", "kept-anchor"] {
            assert!(got.contains(want), "{want} missing from {got:?}");
        }
        assert!(!got.contains("not-a-heading"), "{got:?}");
    }

    #[test]
    fn test_links_finds_inline_image_and_reference_links_outside_code() {
        let md = "See [a](a.md#x) and ![img](images/p.png \"title\").\n\
                  `[not](code.md)`\n\
                  ```\n[no](fenced.md)\n```\n\
                  [ref]: <docs/b.md#y>\n\
                  [web](https://example.com) [here](#local)\n";
        let got: Vec<String> = links(md).into_iter().map(|(_, t)| t).collect();
        assert_eq!(got, vec!["a.md#x", "images/p.png", "docs/b.md#y", "https://example.com", "#local"]);
        assert_eq!(links(md)[0].0, 1);
    }

    fn check(files: &[(&str, &str)], extra: &[&str]) -> Vec<String> {
        let map: BTreeMap<String, String> = files.iter().map(|(p, m)| (p.to_string(), m.to_string())).collect();
        let paths: HashSet<String> = files.iter().map(|(p, _)| p.to_string()).chain(extra.iter().map(|p| p.to_string())).collect();
        broken_links(&map, &|p: &str| paths.contains(p) || paths.iter().any(|f| f.starts_with(&format!("{p}/"))))
    }

    #[test]
    fn test_working_links_pass() {
        let files = [
            ("README.md", "[docs](docs/a.md#part-two) [dir](docs) [src](src/main.rs#L3) [web](https://example.com)"),
            ("docs/a.md", "# A\n\n## Part two\n\n[up](../README.md) [self](#a) [b](sub/b.md)"),
            ("docs/sub/b.md", "[gh](https://github.com/cconstantine/smelt/blob/main/docs/a.md#part-two)"),
        ];
        assert_eq!(check(&files, &["src/main.rs"]), Vec::<String>::new());
    }

    #[test]
    fn test_a_missing_file_is_reported() {
        let got = check(&[("docs/a.md", "line one\n[gone](missing.md)")], &[]);
        assert_eq!(got, vec!["docs/a.md:2: docs/missing.md doesn't exist"]);
    }

    #[test]
    fn test_a_missing_anchor_is_reported() {
        let files = [("docs/a.md", "# A\n[x](b.md#nowhere) [y](#a) [z](#gone)"), ("docs/b.md", "# B")];
        let got = check(&files, &[]);
        assert_eq!(
            got,
            vec!["docs/a.md:2: docs/b.md has no anchor #nowhere", "docs/a.md:2: docs/a.md has no anchor #gone"]
        );
    }

    #[test]
    fn test_a_github_link_into_the_repo_is_checked() {
        let files = [("docs/a.md", "[x](<https://github.com/cconstantine/smelt/blob/main/docs/b.md#moved>)"), ("docs/b.md", "# B")];
        assert_eq!(check(&files, &[]), vec!["docs/a.md:1: docs/b.md has no anchor #moved"]);
    }
}
