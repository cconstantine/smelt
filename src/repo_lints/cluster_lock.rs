//! Every `#[sqlx::test]` takes the cluster lock (SME-212): it names one of
//! `src/test_lock.rs`'s guarded migrators, whose `Deref` refuses the test
//! before sqlx connects unless `scripts/with-cluster-lock` runs it. A bare
//! attribute would run against the shared test Postgres from a plain
//! `cargo test` and drop another run's databases; this finds it in the gate,
//! with no lock or database needed.
//!
//! A small tokenizer reads the whole file (review 2), on the component
//! lint's `classify`: it leaves out `//` and `/* */` comments (nested too),
//! and reads a string, raw string or char literal as one token, so none of
//! them hides or fakes an attribute. Any `#[…]` that names the path
//! `sqlx::test`, wherever it is on its line (`#[::sqlx::test]` and a
//! `cfg_attr` included), counts, and is guarded only when its own brackets
//! hold `migrator = "<a guarded migrator>"`. A `use` that imports
//! `sqlx::test`, read to its `;` through any groups, is a finding too: the
//! import shadows the prelude's `#[test]` (review 1).

use super::components::{classify, Kind};

/// The migrators that check the lock, as the attribute's string names them.
pub(crate) const GUARDED: &[&str] = &["crate::test_lock::MIGRATOR", "crate::test_lock::NO_MIGRATIONS"];

#[derive(Debug, PartialEq)]
enum Token {
    Word(String),
    Punct(char),
    /// A string literal's text between its quotes, escapes as written.
    Str(String),
}

/// `source`'s tokens, each with its line (from 1), from `classify`'s
/// reading of it. Comments, whitespace and char literals leave no token.
fn tokens(source: &str) -> Vec<(usize, Token)> {
    let (chars, ident) = (source.chars().collect::<Vec<_>>(), |c: char| c.is_alphanumeric() || c == '_');
    let kinds = classify(&chars);
    let (mut found, mut i, mut line) = (Vec::new(), 0, 1);
    while i < chars.len() {
        let start = i;
        let run = |more: &dyn Fn(usize) -> bool| (start..chars.len()).find(|&j| !more(j)).unwrap_or(chars.len());
        match kinds[i] {
            Kind::Comment => i = run(&|j| kinds[j] == Kind::Comment),
            Kind::Str => {
                i = run(&|j| kinds[j] == Kind::Str);
                let text: String = chars[start..i].iter().collect();
                // `"…"`, `b"…"` or `r#"…"#`; a char literal is none of them.
                if let Some(value) = text.trim_start_matches(['r', 'b']).trim_start_matches('#').strip_prefix('"') {
                    let value = value.trim_end_matches('#');
                    found.push((line, Token::Str(value.strip_suffix('"').unwrap_or(value).to_string())));
                }
            }
            Kind::Code if ident(chars[i]) => {
                i = run(&|j| kinds[j] == Kind::Code && ident(chars[j]));
                found.push((line, Token::Word(chars[start..i].iter().collect())));
            }
            Kind::Code => {
                if !chars[i].is_whitespace() {
                    found.push((line, Token::Punct(chars[i])));
                }
                i += 1;
            }
        }
        line += chars[start..i].iter().filter(|c| **c == '\n').count();
    }
    found
}

/// Whether `token` is the word `word`.
fn is_word(token: &Token, word: &str) -> bool {
    matches!(token, Token::Word(w) if w == word)
}

/// Each `#[sqlx::test]` attribute in `source`, and each `use` of
/// `sqlx::test`: its line (from 1) and whether it names a guarded migrator
/// (an import never does).
pub(crate) fn sqlx_tests(source: &str) -> Vec<(usize, bool)> {
    let (lines, tokens): (Vec<usize>, Vec<Token>) = tokens(source).into_iter().unzip();
    let mut found = Vec::new();
    for (k, token) in tokens.iter().enumerate() {
        let (line, rest) = (lines[k], &tokens[k + 1..]);
        if *token == Token::Punct('#') && rest.first() == Some(&Token::Punct('[')) {
            let mut depth = 0i32;
            let end = rest.iter().position(|t| {
                depth += match t {
                    Token::Punct('[') => 1,
                    Token::Punct(']') => -1,
                    _ => 0,
                };
                depth == 0
            });
            let inner = &rest[..end.unwrap_or(rest.len())];
            let names = inner.windows(4).any(|w| {
                is_word(&w[0], "sqlx") && w[1] == Token::Punct(':') && w[2] == Token::Punct(':') && is_word(&w[3], "test")
            });
            let guarded = inner.windows(3).any(|w| {
                is_word(&w[0], "migrator") && w[1] == Token::Punct('=') && matches!(&w[2], Token::Str(v) if GUARDED.contains(&v.as_str()))
            });
            if names {
                found.push((line, guarded));
            }
        } else if is_word(token, "use") {
            let end = rest.iter().position(|t| *t == Token::Punct(';')).unwrap_or(rest.len());
            if imports_sqlx_test(&rest[..end]) {
                found.push((line, false));
            }
        }
    }
    found
}

/// Whether the use tree `tree` (after `use`, up to its `;`) imports
/// `sqlx::test`, alone, renamed or in a group, nested groups included.
fn imports_sqlx_test(tree: &[Token]) -> bool {
    let (mut path, mut groups, mut alias) = (Vec::<&str>::new(), Vec::new(), false);
    for token in tree {
        let ends_a_tree = matches!(token, Token::Punct(',' | '}')) || is_word(token, "as");
        if ends_a_tree && path.ends_with(&["sqlx", "test"]) {
            return true;
        }
        match token {
            Token::Word(_) if alias => alias = false,
            Token::Word(w) if w == "as" => alias = true,
            Token::Word(w) if w != "self" => path.push(w.as_str()),
            Token::Punct('{') => groups.push(path.len()),
            Token::Punct(',') => path.truncate(groups.last().copied().unwrap_or(0)),
            Token::Punct('}') => path.truncate(groups.pop().unwrap_or(0)),
            _ => {}
        }
    }
    path.ends_with(&["sqlx", "test"])
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

    /// Review 2, finding 1: rustfmt splits a long group across lines.
    #[test]
    fn test_a_use_split_across_lines_is_read_to_its_semicolon() {
        for import in [
            "use sqlx::{\n    PgPool,\n    test,\n};",
            "use sqlx::{PgPool, /* a note */ test};",
            "use {sqlx::{Row, {test as t}}, std::fmt};",
        ] {
            let text = format!("mod m {{\n    {import}\n}}\n");
            assert_eq!(unguarded("src/a.rs", &text), vec!["src/a.rs:2".to_string()], "{import}");
        }
        for fine in ["use sqlx::{\n    PgPool,\n    testing::TestArgs,\n};", "use other::{sqlx, test};"] {
            assert!(sqlx_tests(&format!("{fine}\n")).is_empty(), "{fine}");
        }
    }

    /// Review 2, finding 2: a block comment is not the attribute's text.
    #[test]
    fn test_a_guard_in_a_block_comment_does_not_count() {
        for attribute in [format!("#[sqlx::test /* {GUARD} */]"), format!("#[sqlx::test( /* /* {GUARD} */ */ )]")] {
            assert_eq!(sqlx_tests(&source(&attribute)), vec![(3, false)], "{attribute}");
        }
    }

    /// Review 2, finding 3: an attribute anywhere in a line counts.
    #[test]
    fn test_an_attribute_after_code_on_its_line_is_a_finding() {
        let bare = attr("");
        for line in [
            format!("fn a() {{}} {bare} async fn t(pool: PgPool) {{}}"),
            format!("/** doc */ {bare}"),
            format!("m!({bare} async fn t(pool: PgPool) {{}});"),
        ] {
            assert_eq!(sqlx_tests(&source(&line)), vec![(3, false)], "{line}");
        }
    }

    /// Review 2, finding 4: raw strings and char literals end where Rust
    /// ends them, and hold no attribute.
    #[test]
    fn test_raw_strings_and_char_literals_end_where_rust_ends_them() {
        let bare = attr("");
        for attribute in [
            format!("#[sqlx::test(fixtures(r#\"a\"b\"#))]\n/// {GUARD}]"),
            format!("#[sqlx::test(fixtures(r\"a\\\"))]\nconst S: &str = \"{GUARD}]\";"),
            format!("let q = '\"'; let l: &'static str = \"\"; {bare}"),
        ] {
            assert_eq!(sqlx_tests(&source(&attribute)), vec![(3, false)], "{attribute}");
        }
        let hidden = format!("const S: &str = r#\"\n{bare}\n\"#;\n/*\n{bare}\n*/\n");
        assert!(sqlx_tests(&hidden).is_empty(), "{hidden}");
    }
}
