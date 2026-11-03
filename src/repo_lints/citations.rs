//! A code comment's citation of a repo path resolves (SME-340). On
//! SME-334 a comment cited `turn/wake.rs`, a file no commit ever had, and
//! three review rounds certified it: reviewers check logic, not whether a
//! cited path exists. This is the floor under that: a `.rs` path written
//! in a Rust comment must name a file the repo has, or had.
//!
//! A citation is a token in a `//` comment that holds a `/` and ends in
//! `.rs` (an optional `:line` after it is ignored). It resolves when some
//! known path — tracked now, or in any commit when the clone has its
//! history — is it, ends in `/` and it, or is its module twin (`x.rs` and
//! `x/mod.rs` are one module, so a file later split into a directory still
//! resolves without history). Left alone:
//!
//! - absolute paths (`/workspace/a.rs`) and `~/` ones: not the repo's;
//! - URLs, and tokens holding `*`, `{`, `<` or `$` (patterns, placeholders);
//! - a line holding `citation: external` (another project's source, such
//!   as tokio's) or `citation: historical` (a path meant from a past life).
//!
//! Not handled: C raw strings (`cr"…"`), which `classify` reads as
//! ordinary ones, and a path only a merge commit introduced, which
//! `git log --name-only` doesn't list (review 1, L4/L5; neither occurs).
//!
//! It is a floor, not a fix: it catches an invented path in seconds and
//! nothing at all for an invented mechanism. Prose and docs are out: they
//! quote paths that move.

use super::components::{classify, Kind};

/// The `.rs` paths cited in `source`'s `//` comments, with their 1-based
/// line numbers, as written (a leading `./` dropped).
pub(crate) fn citations(source: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for (index, comment) in comment_lines(source).iter().enumerate() {
        if comment.is_empty() {
            continue;
        }
        if comment.contains("citation: external") || comment.contains("citation: historical") {
            continue;
        }
        for raw in comment.split(|c: char| c.is_whitespace() || "`'\"()[],;\u{2014}".contains(c)) {
            if raw.contains("://") {
                continue;
            }
            // `see:src/x.rs:10` and `src/x.rs#L10`: the part that is the path
            // (review 1, L1).
            let Some(token) = raw.split(':').map(|part| part.split('#').next().unwrap_or_default()).find(|part| {
                part.trim_matches(['*', '<', '>']).trim_end_matches(['.', '!', '?']).ends_with(".rs")
            }) else {
                continue;
            };
            // Markdown's `**path**` and rustdoc's `<path>` around it.
            let token = token.trim_matches(['*', '<', '>']).trim_end_matches(['.', '!', '?']);
            // `~/` is a home directory, `/` a sandbox's: neither is the repo's.
            if !token.contains('/') || token.starts_with('/') || token.starts_with("~/") {
                continue;
            }
            if token.contains(['*', '{', '<', '$']) {
                continue;
            }
            let mut token = token;
            while let Some(rest) = token.strip_prefix("./").or_else(|| token.strip_prefix("../")) {
                token = rest;
            }
            found.push((index + 1, token.to_string()));
        }
    }
    found
}

/// Each line's comment text alone, on `components`' `classify`: a `//`
/// inside a string (`"file:///…"`) isn't a comment.
fn comment_lines(source: &str) -> Vec<String> {
    let chars: Vec<char> = source.chars().collect();
    let kinds = classify(&chars);
    let mut lines = vec![String::new()];
    for (c, kind) in chars.iter().zip(&kinds) {
        if *c == '\n' {
            lines.push(String::new());
        } else if *kind == Kind::Comment {
            if let Some(line) = lines.last_mut() {
                line.push(*c);
            }
        }
    }
    lines
}

/// Whether `citation` names a path `known` holds: as written, as any
/// path's tail after a `/`, or as its module twin either way.
pub(crate) fn resolves(citation: &str, known: &dyn Fn(&dyn Fn(&str) -> bool) -> bool) -> bool {
    // `x/mod.rs`'s twin is `x.rs` only while `x` keeps a directory: a bare
    // `foo.rs` would match any `*/foo.rs` in the tree (review 1, L3).
    let twin = match citation.strip_suffix("/mod.rs") {
        Some(module) if module.contains('/') => format!("{module}.rs"),
        Some(_) => citation.to_string(),
        None => format!("{}/mod.rs", citation.trim_end_matches(".rs")),
    };
    known(&|path: &str| {
        [citation, twin.as_str()]
            .iter()
            .any(|cited| path == *cited || path.ends_with(&format!("/{cited}")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known_in(paths: &'static [&'static str]) -> impl Fn(&dyn Fn(&str) -> bool) -> bool {
        move |matches: &dyn Fn(&str) -> bool| paths.iter().any(|p| matches(p))
    }

    #[test]
    fn test_a_comment_names_its_citations() {
        let source = "fn f() {} // see `src/turn/run.rs:120` and (turn/state.rs).\nlet s = \"src/not/a/comment.rs\";\nlet u = \"file:///w/my app/a.rs\";\n";
        assert_eq!(
            citations(source),
            vec![(1, "src/turn/run.rs".to_string()), (1, "turn/state.rs".to_string())]
        );
    }

    #[test]
    fn test_a_citation_in_markup_or_with_a_prefix_is_still_one() {
        let source = "// **src/a/b.rs** <src/c/d.rs> src/e/f.rs#L10 src/g/h.rs\u{2014}then\n\
                      // see:src/i/j.rs:3 and ../src/k/l.rs\n";
        let cited: Vec<String> = citations(source).into_iter().map(|(_, c)| c).collect();
        assert_eq!(cited, ["src/a/b.rs", "src/c/d.rs", "src/e/f.rs", "src/g/h.rs", "src/i/j.rs", "src/k/l.rs"]);
    }

    #[test]
    fn test_what_isnt_a_repo_citation_is_left_alone() {
        let source = "// /workspace/a.rs and https://github.com/x/y/blob/main/src/lib.rs\n\
                      // src/*.rs, src/{a}/mod.rs, $DIR/x.rs, main.rs, ~/notes/x.rs\n\
                      // tokio's runtime/blocking/pool.rs (citation: external)\n\
                      // the old src/gone.rs (citation: historical)\n";
        assert_eq!(citations(source), Vec::<(usize, String)>::new());
    }

    #[test]
    fn test_a_citation_resolves_as_written_by_its_tail_or_as_its_module_twin() {
        let known = known_in(&["src/turn/run.rs", "src/sandbox/mod.rs", "src/frontend/pages/providers/models.rs"]);
        assert!(resolves("src/turn/run.rs", &known));
        assert!(resolves("turn/run.rs", &known), "a tail after a /");
        assert!(resolves("providers/models.rs", &known));
        assert!(resolves("src/sandbox.rs", &known), "split into a directory since");
        assert!(!resolves("turn/wake.rs", &known), "SME-334's citation");
        assert!(!resolves("un/run.rs", &known), "a tail, not a substring");
        let known = known_in(&["src/sandbox/foo.rs"]);
        assert!(!resolves("foo/mod.rs", &known), "a one-directory module's twin isn't a bare name");
    }
}
