//! Every `#[sqlx::test]` takes the cluster lock (SME-212): it names one of
//! `src/test_lock.rs`'s guarded migrators, whose `Deref` refuses the test
//! before sqlx connects unless `scripts/with-cluster-lock` runs it. A bare
//! attribute would run against the shared test Postgres from a plain
//! `cargo test` and drop another run's databases; this finds it in the gate,
//! with no lock or database needed.
//!
//! It reads lines, not Rust. A line whose code starts with `#[` starts
//! attributes, one or more, each read to its closing `]` (across lines too)
//! with any `//` comment left out; anything else (a comment, a string) isn't
//! one. Any attribute that names `sqlx::test`, `#[::sqlx::test]` and a
//! `cfg_attr` included, counts, and is guarded only when its own brackets
//! name a guarded migrator. A `use` that imports `sqlx::test` is a finding
//! too: the import shadows the prelude's `#[test]` (review 1).

/// The migrators that check the lock, as the attribute names them.
pub(crate) const GUARDED: &[&str] = &[
    r#"migrator = "crate::test_lock::MIGRATOR""#,
    r#"migrator = "crate::test_lock::NO_MIGRATIONS""#,
];

/// Each `#[sqlx::test]` attribute in `source`, and each `use` of
/// `sqlx::test`: its line (from 1) and whether it names a guarded migrator
/// (an import never does).
pub(crate) fn sqlx_tests(source: &str) -> Vec<(usize, bool)> {
    let lines: Vec<&str> = source.lines().collect();
    let mut found = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let code = line.trim_start();
        if code.starts_with("#[") {
            for attribute in attributes(&lines[i..]).iter().filter(|a| names_sqlx_test(a)) {
                found.push((i + 1, GUARDED.iter().any(|g| attribute.contains(g))));
            }
        } else if imports_sqlx_test(code) {
            found.push((i + 1, false));
        }
    }
    found
}

/// The attributes that start `lines[0]`, each from `#[` to its own closing
/// `]`, read across lines while one is open, with `//` comments outside a
/// string left out. One still open when the source ends runs to there.
fn attributes(lines: &[&str]) -> Vec<String> {
    let (mut found, mut open, mut depth) = (Vec::new(), None::<String>, 0usize);
    let (mut in_string, mut escaped) = (false, false);
    for line in lines {
        let chars: Vec<char> = line.chars().collect();
        let mut j = 0;
        while j < chars.len() {
            let (c, next) = (chars[j], chars.get(j + 1).copied());
            j += 1;
            let Some(attribute) = open.as_mut() else {
                match (c, next) {
                    (' ' | '\t', _) => continue,
                    ('#', Some('[')) => {
                        open = Some("#[".to_string());
                        depth = 1;
                        j += 1;
                        continue;
                    }
                    // Code or a comment after the attributes.
                    _ => return found,
                }
            };
            if in_string {
                attribute.push(c);
                match c {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => in_string = false,
                    _ => {}
                }
                continue;
            }
            match (c, next) {
                ('/', Some('/')) => break,
                ('"', _) => in_string = true,
                ('[', _) => depth += 1,
                (']', _) => depth -= 1,
                _ => {}
            }
            attribute.push(c);
            if depth == 0 {
                found.extend(open.take());
            }
        }
        match open.as_mut() {
            Some(attribute) => attribute.push('\n'),
            None => return found,
        }
    }
    found.extend(open);
    found
}

/// Whether `text` names the path `sqlx::test` (`::sqlx::test` included),
/// not a longer name such as `sqlx::testing`.
fn names_sqlx_test(text: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices("sqlx::test").any(|(at, m)| {
        !text[..at].ends_with(ident) && !text[at + m.len()..].starts_with(ident)
    })
}

/// Whether the line `code` is a `use` that imports `sqlx::test`, alone,
/// renamed or in a `sqlx::{..}` group.
fn imports_sqlx_test(code: &str) -> bool {
    let code = code.split("//").next().unwrap_or_default();
    let mut words = code.split_whitespace();
    let first = words.next().unwrap_or_default();
    let is_use = first == "use" || (first.starts_with("pub") && words.next() == Some("use"));
    if !is_use {
        return false;
    }
    let grouped = code.split_once("sqlx::{").is_some_and(|(_, rest)| {
        let group = rest.split('}').next().unwrap_or_default();
        group.split(',').any(|item| {
            let item = item.trim();
            item == "test" || item.starts_with("test ")
        })
    });
    names_sqlx_test(code) || grouped
}

/// `path:line` for each attribute in `source` that doesn't take the lock.
pub(crate) fn unguarded(path: &str, source: &str) -> Vec<String> {
    sqlx_tests(source)
        .into_iter()
        .filter(|(_, guarded)| !guarded)
        .map(|(line, _)| format!("{path}:{line}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture's attribute line, built so no line of this file starts
    /// with one.
    fn attr(args: &str) -> String {
        format!("#[sqlx::test{args}]")
    }

    fn source(attribute: &str) -> String {
        format!("use super::*;\n\n    {attribute}\nasync fn test_x(pool: PgPool) {{}}\n")
    }

    #[test]
    fn test_a_bare_attribute_is_a_finding() {
        assert_eq!(unguarded("src/a.rs", &source(&attr(""))), vec!["src/a.rs:3".to_string()]);
    }

    #[test]
    fn test_the_guarded_migrators_are_fine() {
        for name in ["MIGRATOR", "NO_MIGRATIONS"] {
            let attribute = attr(&format!(r#"(migrator = "crate::test_lock::{name}")"#));
            assert_eq!(unguarded("src/a.rs", &source(&attribute)), Vec::<String>::new(), "{name}");
        }
        let with_fixtures = attr(r#"(migrator = "crate::test_lock::MIGRATOR", fixtures("users"))"#);
        assert_eq!(unguarded("src/a.rs", &source(&with_fixtures)), Vec::<String>::new());
    }

    #[test]
    fn test_another_migrator_or_none_is_a_finding() {
        for args in [r#"(migrator = "crate::MIGRATOR")"#, "(migrations = false)", r#"(migrations = "other")"#] {
            assert_eq!(unguarded("src/a.rs", &source(&attr(args))), vec!["src/a.rs:3".to_string()], "{args}");
        }
    }

    #[test]
    fn test_a_mention_in_a_comment_or_string_is_not_an_attribute() {
        let bare = attr("");
        let text = format!("/// Each {bare} takes the lock.\n// {bare}\nlet s = \"{bare}\";\n");
        assert!(sqlx_tests(&text).is_empty(), "{text}");
    }

    const GUARD: &str = r#"migrator = "crate::test_lock::MIGRATOR""#;

    /// Review 1: spellings that still compile to an unguarded sqlx test.
    #[test]
    fn test_every_spelling_of_an_unguarded_sqlx_test_is_a_finding() {
        let bare = attr("");
        for attribute in [
            "#[::sqlx::test]".to_string(),
            r#"#[cfg_attr(feature = "server", sqlx::test)]"#.to_string(),
            format!("#[ignore] {bare}"),
            format!("#[ignore] #[should_panic] {bare}"),
            format!("{bare} // {GUARD}"),
            format!("#[sqlx::test( // {GUARD}\n)]"),
            format!("{bare} #[doc = \"{}\"]", GUARD.replace('"', "\\\"")),
            "#[cfg_attr(\n    feature = \"server\",\n    sqlx::test\n)]".to_string(),
        ] {
            assert_eq!(unguarded("src/a.rs", &source(&attribute)), vec!["src/a.rs:3".to_string()], "{attribute}");
        }
    }

    #[test]
    fn test_every_spelling_of_a_guarded_sqlx_test_is_fine() {
        let guarded = attr(&format!("({GUARD})"));
        for attribute in [
            format!("#[::sqlx::test({GUARD})]"),
            format!("#[cfg_attr(feature = \"server\", sqlx::test({GUARD}))]"),
            format!("#[ignore] {guarded}"),
            format!("{guarded} #[ignore] // a note"),
            format!("#[sqlx::test(\n    {GUARD},\n)]"),
        ] {
            assert_eq!(sqlx_tests(&source(&attribute)), vec![(3, true)], "{attribute}");
        }
    }

    #[test]
    fn test_importing_sqlx_test_is_a_finding() {
        for import in [
            "use sqlx::test;",
            "use ::sqlx::test;",
            "use sqlx::test as sqlx_test;",
            "pub(crate) use sqlx::test;",
            "use sqlx::{PgPool, test};",
            "use sqlx::{test as t, PgPool};",
        ] {
            let text = format!("mod m {{\n    {import}\n}}\n");
            assert_eq!(unguarded("src/a.rs", &text), vec!["src/a.rs:2".to_string()], "{import}");
        }
        for fine in ["use sqlx::PgPool;", "use sqlx::testing::TestArgs;", "use sqlx::{PgPool, Row};", "// use sqlx::test;"] {
            assert!(sqlx_tests(&format!("{fine}\n")).is_empty(), "{fine}");
        }
    }
}
