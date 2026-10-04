//! Syntax highlighting for code blocks in the model's replies (SME-30):
//! syntect with two-face's grammar set (bat's extra grammars), producing a
//! tree of spans carrying class names (`hl-keyword` and so on), never
//! inline colours, so `assets/highlight.css` themes them in light and dark.

use std::sync::OnceLock;

use syntect::parsing::{BasicScopeStackOp, ParseState, Scope, ScopeStack, SyntaxSet};
use syntect::util::LinesWithEndings;

/// Highlighted code: text, or a span whose class names come from a
/// syntax scope (`keyword.control.rust` becomes
/// `hl-keyword hl-control hl-rust`), with its contents.
#[derive(Debug, Clone, PartialEq)]
pub enum Span {
    Text(String),
    Scoped { classes: String, children: Vec<Span> },
}

/// Code longer than this shows plain: highlighting runs in the browser on
/// every render of a reply, and syntect's pure-Rust regex engine can be
/// slow on large inputs.
pub const MAX_HIGHLIGHT_BYTES: usize = 64 * 1024;

/// `code` highlighted as `lang` (a fence's info string: a language name
/// or file extension, matched case-insensitively), or `None` when the
/// language isn't known, the code is too long, or a grammar fails on it.
pub fn highlight(lang: &str, code: &str) -> Option<Vec<Span>> {
    if lang.is_empty() || code.len() > MAX_HIGHLIGHT_BYTES {
        return None;
    }
    let syntaxes = syntaxes();
    let syntax = syntaxes
        .find_syntax_by_token(lang)
        .or_else(|| syntaxes.find_syntax_by_token(&lang.to_ascii_lowercase()))?;
    let mut state = ParseState::new(syntax);
    let mut scopes = ScopeStack::new();
    // Open spans, innermost last; the first is the root.
    let mut open: Vec<(String, Vec<Span>)> = vec![(String::new(), Vec::new())];
    for line in LinesWithEndings::from(code) {
        let ops = state.parse_line(line, syntaxes).ok()?;
        let mut at = 0;
        for (index, op) in ops {
            if index > at {
                push_text(&mut open, line.get(at..index)?);
                at = index;
            }
            scopes
                .apply_with_hook(&op, |basic, _| match basic {
                    BasicScopeStackOp::Push(scope) => open.push((classes_for(scope), Vec::new())),
                    BasicScopeStackOp::Pop => close_span(&mut open),
                })
                .ok()?;
        }
        push_text(&mut open, line.get(at..)?);
    }
    while open.len() > 1 {
        close_span(&mut open);
    }
    open.pop().map(|(_, root)| root)
}

/// two-face's grammar set (with newlines), loaded the first time a code
/// block is highlighted and kept for the process (or page).
fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

/// A scope's class names: each of its atoms with an `hl-` prefix.
fn classes_for(scope: Scope) -> String {
    scope.build_string().split('.').map(|atom| format!("{CLASS_PREFIX}{atom}")).collect::<Vec<_>>().join(" ")
}

/// The prefix of every highlighting class (and of `hl-code`, the class of
/// a highlighted block, which carries the theme's colours).
pub const CLASS_PREFIX: &str = "hl-";

fn push_text(open: &mut [(String, Vec<Span>)], text: &str) {
    if text.is_empty() {
        return;
    }
    let Some((_, children)) = open.last_mut() else {
        return;
    };
    if let Some(Span::Text(last)) = children.last_mut() {
        last.push_str(text);
    } else {
        children.push(Span::Text(text.to_string()));
    }
}

/// Closes the innermost open span into its parent, dropping it if empty.
/// The root is never closed.
fn close_span(open: &mut Vec<(String, Vec<Span>)>) {
    if open.len() < 2 {
        return;
    }
    let Some((classes, children)) = open.pop() else {
        return;
    };
    if children.is_empty() {
        return;
    }
    if let Some((_, parent)) = open.last_mut() {
        parent.push(Span::Scoped { classes, children });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text of a highlighted tree, in order.
    fn flatten(spans: &[Span]) -> String {
        spans
            .iter()
            .map(|s| match s {
                Span::Text(t) => t.clone(),
                Span::Scoped { children, .. } => flatten(children),
            })
            .collect()
    }

    /// Every class name in a tree.
    fn classes(spans: &[Span]) -> Vec<String> {
        let mut out = Vec::new();
        for span in spans {
            if let Span::Scoped { classes: c, children } = span {
                out.extend(c.split(' ').map(str::to_string));
                out.extend(classes(children));
            }
        }
        out
    }

    #[test]
    fn test_rust_highlights_keywords_and_keeps_the_text() {
        let code = "fn main() {\n    let x = \"hi\";\n}\n";
        let spans = highlight("rust", code).expect("rust is a known language");
        assert_eq!(flatten(&spans), code);
        let classes = classes(&spans);
        assert!(classes.iter().any(|c| c == "hl-keyword" || c == "hl-storage"), "{classes:?}");
        assert!(classes.iter().any(|c| c == "hl-string"), "{classes:?}");
        assert!(classes.iter().all(|c| c.starts_with("hl-")), "{classes:?}");
    }

    #[test]
    fn test_two_faces_extra_grammars_and_extensions_are_known() {
        for lang in ["typescript", "ts", "toml", "Dockerfile", "python", "py", "yaml", "go", "bash", "TSX"] {
            assert!(highlight(lang, "x\n").is_some(), "{lang} should be a known language");
        }
    }

    #[test]
    fn test_an_unknown_language_and_too_long_code_show_plain() {
        assert_eq!(highlight("no-such-language", "x\n"), None);
        assert_eq!(highlight("", "x\n"), None);
        let long = "let x = 1;\n".repeat(MAX_HIGHLIGHT_BYTES / 10);
        assert_eq!(highlight("rust", &long), None);
    }

    #[test]
    fn test_code_without_a_final_newline_and_non_ascii_keep_their_text() {
        let code = "s = \"héllo 你好\" # 🎉";
        let spans = highlight("python", code).expect("python is known");
        assert_eq!(flatten(&spans), code);
    }

    /// What `assets/highlight.css` should hold: two-face's OneHalfLight
    /// theme, and OneHalfDark when the system is in dark mode (as
    /// `assets/chat.css` does), with smelt's class prefix.
    fn generated_css() -> String {
        use syntect::html::{ClassStyle, css_for_theme_with_class_style};
        use two_face::theme::EmbeddedThemeName;
        let themes = two_face::theme::extra();
        let style = ClassStyle::SpacedPrefixed { prefix: CLASS_PREFIX };
        let light = css_for_theme_with_class_style(themes.get(EmbeddedThemeName::OneHalfLight), style)
            .expect("the light theme's CSS");
        let dark = css_for_theme_with_class_style(themes.get(EmbeddedThemeName::OneHalfDark), style)
            .expect("the dark theme's CSS");
        format!(
            "/* Generated by src/highlight.rs's test_highlight_css_matches_the_themes:\n * edit the themes there, then run it with SMELT_UPDATE_HIGHLIGHT_CSS=1. */\n\n{light}\n@media (prefers-color-scheme: dark) {{\n{dark}}}\n"
        )
    }

    #[test]
    fn test_highlight_css_matches_the_themes() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/highlight.css");
        let expected = generated_css();
        if std::env::var("SMELT_UPDATE_HIGHLIGHT_CSS").is_ok_and(|v| v == "1") {
            std::fs::write(path, &expected).expect("write assets/highlight.css");
        }
        let actual = std::fs::read_to_string(path).expect("read assets/highlight.css");
        assert!(
            actual == expected,
            "assets/highlight.css doesn't match the themes; regenerate it with SMELT_UPDATE_HIGHLIGHT_CSS=1"
        );
        // Every rule is scoped to smelt's prefix, so the theme can't style
        // anything else on the page.
        assert!(!actual.contains(".code"), "an unprefixed .code rule");
    }

    #[test]
    fn test_an_empty_block_is_empty() {
        assert_eq!(highlight("rust", "").map(|s| flatten(&s)), Some(String::new()));
    }
}
