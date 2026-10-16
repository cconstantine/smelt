//! `#[component]` functions stay under the function-length limit too
//! (SME-163). clippy's `too_many_lines` holds production functions to 200
//! code lines, but it can't see inside a component: the macro rewrites the
//! function. This counts a component's lines the way clippy counts a
//! function's: every line of its body but blank ones and those holding only
//! a comment.

/// The most code lines a component's body may hold, clippy's threshold.
pub(crate) const MAX_COMPONENT_LINES: usize = 200;

/// Each `#[component]` function in `source`: its name and the code lines in
/// its body. A component whose function or body can't be found is an error
/// naming its line, so the check fails rather than leaving it out.
pub(crate) fn component_lengths(source: &str) -> Result<Vec<(String, usize)>, String> {
    let chars: Vec<char> = source.chars().collect();
    let kinds = classify(&chars);
    // Each line's code: its characters outside comments, strings kept.
    let mut lines: Vec<String> = vec![String::new()];
    for (c, kind) in chars.iter().zip(&kinds) {
        if *c == '\n' {
            lines.push(String::new());
        } else if *kind != Kind::Comment {
            lines.last_mut().expect("a line").push(*c);
        }
    }
    // Braces count only in code proper, not in strings.
    let mut code_only: Vec<String> = vec![String::new()];
    for (c, kind) in chars.iter().zip(&kinds) {
        if *c == '\n' {
            code_only.push(String::new());
        } else {
            code_only.last_mut().expect("a line").push(if *kind == Kind::Code { *c } else { ' ' });
        }
    }
    let mut found = Vec::new();
    let mut i = 0;
    while i < code_only.len() {
        if !is_component_attribute(&code_only[i]) {
            i += 1;
            continue;
        }
        // The function's name (on the attribute's line, or a later one), then
        // its body from the first `{`.
        let Some((fn_line, name)) = (i..code_only.len()).find_map(|k| fn_name(&code_only[k]).map(|n| (k, n))) else {
            return Err(format!("no function after the component attribute at line {}", i + 1));
        };
        let mut depth = 0i64;
        let mut open = None;
        let mut close = None;
        'scan: for (k, line) in code_only.iter().enumerate().skip(fn_line) {
            for c in line.chars() {
                match c {
                    '{' => {
                        depth += 1;
                        open.get_or_insert(k);
                    }
                    '}' => {
                        depth -= 1;
                        if depth == 0 && open.is_some() {
                            close = Some(k);
                            break 'scan;
                        }
                    }
                    _ => {}
                }
            }
        }
        let (Some(open), Some(close)) = (open, close) else {
            return Err(format!("couldn't find the body of component {name} at line {}", fn_line + 1));
        };
        let count = lines[open + 1..close].iter().filter(|l| !l.trim().is_empty()).count();
        found.push((name, count));
        i = close + 1;
    }
    Ok(found)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Code,
    Comment,
    Str,
}

/// Each character's kind: in a comment, in a string or char literal, or code.
fn classify(chars: &[char]) -> Vec<Kind> {
    let mut kinds = vec![Kind::Code; chars.len()];
    let at = |i: usize, s: &str| s.chars().enumerate().all(|(k, c)| chars.get(i + k) == Some(&c));
    let mut i = 0;
    while i < chars.len() {
        let start = i;
        let kind;
        if at(i, "//") {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            kind = Kind::Comment;
        } else if at(i, "/*") {
            let mut depth = 0;
            loop {
                if at(i, "/*") {
                    depth += 1;
                    i += 2;
                } else if at(i, "*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else if i >= chars.len() {
                    break;
                } else {
                    i += 1;
                }
            }
            kind = Kind::Comment;
        } else if (chars[i] == 'r' || at(i, "br")) && is_raw_string_start(chars, i) {
            i += if chars[i] == 'b' { 2 } else { 1 };
            let mut hashes = 0;
            while chars.get(i) == Some(&'#') {
                hashes += 1;
                i += 1;
            }
            i += 1; // the opening quote
            let closing: String = std::iter::once('"').chain(std::iter::repeat_n('#', hashes)).collect();
            while i < chars.len() && !at(i, &closing) {
                i += 1;
            }
            i += closing.chars().count();
            kind = Kind::Str;
        } else if chars[i] == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                i += if chars[i] == '\\' { 2 } else { 1 };
            }
            i += 1;
            kind = Kind::Str;
        } else if chars[i] == '\'' && chars.get(i + 1) == Some(&'\\') {
            // Past the quote, the backslash and the escaped character (which
            // may be a quote itself), then to the closing quote.
            i += 3;
            while i < chars.len() && chars[i] != '\'' {
                i += 1;
            }
            i += 1;
            kind = Kind::Str;
        } else if chars[i] == '\'' && chars.get(i + 2) == Some(&'\'') {
            i += 3;
            kind = Kind::Str;
        } else {
            i += 1;
            continue;
        }
        let end = i.min(chars.len());
        for k in &mut kinds[start..end] {
            *k = kind;
        }
    }
    kinds
}

/// Whether a raw string (`r"`, `r#"`, `br"`) starts at `i`, and not an
/// identifier that happens to end in `r`.
fn is_raw_string_start(chars: &[char], i: usize) -> bool {
    if i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_') {
        return false;
    }
    let mut k = i + if chars[i] == 'b' { 2 } else { 1 };
    while chars.get(k) == Some(&'#') {
        k += 1;
    }
    chars.get(k) == Some(&'"')
}

/// Whether a line starts with the `component` attribute, however it's
/// written: `#[component]`, `#[component(no_case_check)]`,
/// `#[dioxus::prelude::component]`, with the function after it or not.
fn is_component_attribute(line: &str) -> bool {
    let Some(rest) = line.trim_start().strip_prefix("#[") else { return false };
    let path: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':').collect();
    path == "component" || path.ends_with("::component")
}

/// The function a line declares, if it does.
fn fn_name(line: &str) -> Option<String> {
    let rest = line.split_once("fn ")?.1;
    let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_components_code_lines_are_counted() {
        let src = "#[component]\nfn Small() -> Element {\n    let a = 1;\n\n    // a comment\n    rsx! { div {} }\n}\n";
        assert_eq!(component_lengths(src), Ok(vec![("Small".to_string(), 2)]));
    }

    #[test]
    fn test_braces_in_strings_chars_and_comments_dont_end_a_component() {
        let src = concat!(
            "#[component]\n",
            "pub fn Tricky(x: i64) -> Element {\n",
            "    let s = \"}}} not the end\";\n",
            "    let r = r#\"} still not\"#;\n",
            "    let c = '}';\n",
            "    /* } */ let b = '{';\n",
            "    let l: &'static str = \"\";\n",
            "    rsx! { div { \"{x}\" } }\n",
            "}\n",
            "fn after() {}\n",
        );
        assert_eq!(component_lengths(src), Ok(vec![("Tricky".to_string(), 6)]));
    }

    /// Code review 1 (SME-163): an attribute with arguments, a full path, or
    /// the function on the attribute's own line is a component too.
    #[test]
    fn test_every_way_of_writing_the_attribute_is_counted() {
        let body = "    let a = 1;\n    let b = 2;\n}\n";
        for head in [
            "#[component(no_case_check)]\nfn A() -> Element {\n",
            "#[dioxus::prelude::component]\nfn A() -> Element {\n",
            "#[component] fn A() -> Element {\n",
            "  #[component]\n  pub fn A() -> Element {\n",
        ] {
            assert_eq!(component_lengths(&format!("{head}{body}")), Ok(vec![("A".to_string(), 2)]), "{head:?}");
        }
        assert_eq!(component_lengths("#[components]\nfn B() {\n    let a = 1;\n}\n"), Ok(vec![]));
    }

    /// Code review 2 (SME-163): an escaped quote in a char literal (`'\''`)
    /// ends the literal; read as code, its closing quote paired with a later
    /// one and the component's braces were lost.
    #[test]
    fn test_an_escaped_quote_char_doesnt_hide_a_component() {
        let src = "#[component]\nfn Q() -> Element {\n    let q = ['\\'','\"'];\n    let a = 1;\n}\n";
        assert_eq!(component_lengths(src), Ok(vec![("Q".to_string(), 2)]));
    }

    /// A component whose body can't be read fails the check rather than
    /// being left out of it.
    #[test]
    fn test_a_component_whose_body_isnt_found_is_an_error() {
        assert!(component_lengths("#[component]\nfn Open() -> Element {\n    let a = 1;\n").is_err());
        assert!(component_lengths("#[component]\n").is_err());
    }

    #[test]
    fn test_only_components_are_counted() {
        let src = "fn plain() {\n    let a = 1;\n}\n#[component]\npub(crate) fn Two(props: P) -> Element {\n    rsx! {}\n}\n";
        assert_eq!(component_lengths(src), Ok(vec![("Two".to_string(), 1)]));
    }
}
