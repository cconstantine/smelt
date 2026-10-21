//! Every `#[sqlx::test]` takes the cluster lock (SME-212): it names one of
//! `src/test_lock.rs`'s guarded migrators, whose `Deref` refuses the test
//! before sqlx connects unless `scripts/with-cluster-lock` runs it. A bare
//! attribute would run against the shared test Postgres from a plain
//! `cargo test` and drop another run's databases; this finds it in the gate,
//! with no lock or database needed.
//!
//! It reads lines, not Rust: a line whose code starts with `#[sqlx::test` is
//! an attribute, and anything else (a comment, a string) isn't. An attribute
//! split across lines is a finding even when guarded; keep it on one line.

/// The migrators that check the lock, as the attribute names them.
pub(crate) const GUARDED: &[&str] = &[
    r#"migrator = "crate::test_lock::MIGRATOR""#,
    r#"migrator = "crate::test_lock::NO_MIGRATIONS""#,
];

/// Each `#[sqlx::test]` attribute in `source`: its line (from 1) and whether
/// it names a guarded migrator.
pub(crate) fn sqlx_tests(source: &str) -> Vec<(usize, bool)> {
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim_start().starts_with("#[sqlx::test"))
        .map(|(i, line)| (i + 1, GUARDED.iter().any(|g| line.contains(g))))
        .collect()
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
}
