//! No new clock-derived ids from the fold this lint knows (SME-255):
//! `crate::id_alloc` allocates the ids whose values name cluster
//! objects, from one cursor per band. A finding is the identifier
//! `uuid_like` in code, or — within [`WINDOW`] lines of a clock read
//! (`as_nanos`, `as_micros`, `as_millis`, `as_secs`) — a remainder
//! against a numeric literal of six or more digits, written as `%` or
//! as a rem method (`rem_euclid`, `wrapping_rem`, `checked_rem`): the
//! shape every met site was folded in, from `2e9 + nanos % 1e9` to the
//! LSP band's `(seconds % 1e6) * 1000`. It fails the gate on any of
//! those in a tracked file outside [`ALLOWED`].
//!
//! That is the grammar, not the concept: `% (1_000_000_000)` (operands
//! inside parentheses), `% 1e9` (a float literal), `% BAND_WIDTH` (a
//! named constant), `nanos %= …`, and a renamed helper all pass — the
//! one known renamed site, `sandbox_instance_cleanup`'s bare-nanos
//! `unique()`, was fixed and pinned outside this grammar (its tests
//! carry the cross-reference). Comments hold neither finding, on
//! `components`' `classify`; strings do, on purpose: a fold pasted in a
//! string is a fold one `include!` away.

use super::components::{classify, Kind};

/// Files allowed to fold a clock read: the allocator, whose seed mixes
/// nanos with the pid once per process and is the point of the design,
/// and this lint, whose tests carry folds to find.
pub(crate) const ALLOWED: &[&str] = &["src/id_alloc.rs", "src/repo_lints/id_alloc.rs"];

/// The clock reads a fold starts from.
const CLOCK: &[&str] = &["as_nanos", "as_micros", "as_millis", "as_secs"];

/// The rem methods: `%` written as a call. A fold through one has no
/// `%` character at all, so the scan for one alone would miss it.
const REM_METHODS: &[&str] = &["rem_euclid", "wrapping_rem", "checked_rem"];

/// How many lines apart a clock read and its modulo may sit and still
/// read as one fold — three covers the statement-per-line style every
/// met site was written in.
const WINDOW: usize = 3;

/// The code of each line, comments removed, as components renders it.
fn code_lines(source: &str) -> Vec<String> {
    let chars: Vec<char> = source.chars().collect();
    let kinds = classify(&chars);
    let mut lines: Vec<String> = vec![String::new()];
    for (c, kind) in chars.iter().zip(&kinds) {
        if *c == '\n' {
            lines.push(String::new());
        } else if *kind != Kind::Comment {
            lines.last_mut().expect("a line").push(*c);
        }
    }
    lines
}

/// The words of `line`, split on everything but id characters.
fn words(line: &str) -> impl Iterator<Item = &str> {
    line.split(|c: char| !c.is_alphanumeric() && c != '_').filter(|w| !w.is_empty())
}

/// Whether `line` reads the clock.
fn reads_clock(line: &str) -> bool {
    words(line).any(|w| CLOCK.contains(&w))
}

/// Whether `line` takes a remainder of a literal of six or more digits,
/// as an operator or a rem method.
fn folds(line: &str) -> bool {
    let chars: Vec<char> = line.chars().collect();
    for (k, c) in chars.iter().enumerate() {
        if *c != '%' || chars.get(k + 1) == Some(&'=') {
            continue;
        }
        let mut j = k + 1;
        while chars.get(j) == Some(&' ') {
            j += 1;
        }
        if digits_from(&chars, j) >= 6 {
            return true;
        }
    }
    for method in REM_METHODS {
        let mut from = 0;
        while let Some(k) = line[from..].find(method) {
            // The literal is the argument: `method(1_000_000_000)`.
            let after: Vec<char> = line[from + k + method.len()..].chars().collect();
            let mut i = 0;
            while matches!(after.get(i), Some(c) if c.is_whitespace()) {
                i += 1;
            }
            if after.get(i) == Some(&'(') {
                i += 1;
                while matches!(after.get(i), Some(c) if c.is_whitespace()) {
                    i += 1;
                }
                if after[i..].iter().take_while(|c| c.is_ascii_digit() || **c == '_').filter(|c| c.is_ascii_digit()).count() >= 6 {
                    return true;
                }
            }
            from += k + method.len();
        }
    }
    false
}

/// How many decimal digits start at `j` in `chars`, underscores ignored.
fn digits_from(chars: &[char], j: usize) -> usize {
    let mut j = j;
    let mut digits = 0;
    while matches!(chars.get(j), Some(d) if d.is_ascii_digit() || *d == '_') {
        if chars[j].is_ascii_digit() {
            digits += 1;
        }
        j += 1;
    }
    digits
}

/// `path:line` for each fold and each `uuid_like` in `source`.
pub(crate) fn folds_and_helpers(path: &str, source: &str) -> Vec<String> {
    let lines = code_lines(source);
    let mut found = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let folds = folds(line);
        let near_clock = folds
            && lines[i.saturating_sub(WINDOW)..(i + WINDOW + 1).min(lines.len())].iter().any(|l| reads_clock(l));
        if words(line).any(|w| w == "uuid_like") || near_clock {
            found.push(format!("{path}:{}", i + 1));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(source: &str) -> Vec<String> {
        folds_and_helpers("src/a.rs", source)
    }

    /// The met folds, as their sites wrote them.
    #[test]
    fn test_a_fold_is_a_finding() {
        for source in [
            "    let nanos = clock.as_nanos();\n    3_000_000_000 + i64::try_from(nanos % 1_000_000_000).unwrap_or_default()\n",
            "    let seconds = clock.as_secs() as i64;\n    3_000_000_000 + (seconds % 1_000_000) * 1_000 + next\n",
            "    let id = (nanos.as_nanos() % 1_000_000_000) as i64 + 1_000_000_000;\n",
            "    let nanos = clock.as_nanos();\n    3_000_000_000 + nanos.rem_euclid(1_000_000_000)\n",
            "    let id = clock.as_nanos().checked_rem(1_000_000_000) as i64;\n",
            "    let seconds = clock.as_secs();\n    let id = seconds.wrapping_rem(1_000_000);\n",
        ] {
            assert!(!scan(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn test_a_uuid_like_helper_is_a_finding() {
        assert!(!scan("fn uuid_like() -> String {\n    String::new()\n}\n").is_empty());
        assert!(scan("fn uuid_like_not() {}\n").is_empty(), "the name, as a word");
    }

    /// Timing reads, and modulo work on values, stay fine.
    #[test]
    fn test_plain_code_is_fine() {
        for source in [
            "    let id = crate::id_alloc::GLOBAL_IDS.next_base();\n",
            "    let age = now.duration_since(then).as_secs();\n    assert!(age < 60, \"{age} s\");\n",
            "    let small = total % 97;\n    let half = split % 1000;\n",
            "    let m = total.rem_euclid(97);\n    let h = span.checked_rem(60);\n",
            "    fn rem_euclid_safe() {}\n",
        ] {
            assert!(scan(source).is_empty(), "{source}");
        }
    }

    /// A comment about a fold is the docs talking, not a fold.
    #[test]
    fn test_a_comment_holds_no_finding() {
        let source = "// let id = 2_000_000_000 + nanos % 1_000_000_000, once, before SME-255.\nfn id_is_fine() {}\n";
        assert!(scan(source).is_empty());
    }

    /// Beyond the window the fold reads as two unrelated lines: the
    /// 40,000-byte files this guards don't spread folds that far.
    #[test]
    fn test_a_fold_past_the_window_is_not_read_as_one() {
        let source = "    let nanos = clock.as_nanos();\n\n\n\n\n    let id = nanos % 1_000_000_000;\n";
        assert!(scan(source).is_empty());
    }
}
