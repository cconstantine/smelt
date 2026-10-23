//! A Markdown file's YAML frontmatter: a first line of exactly `---`, up
//! to the next line of exactly `---`. Personas use it, and skills (SME-37)
//! will too.

/// `line` without its line ending (`\n` or `\r\n`).
fn without_ending(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

/// `(frontmatter, body)` of `text`. A trailing `\r` on either `---` line
/// is fine, for a checkout with CRLF line endings.
pub(crate) fn split(text: &str) -> Result<(&str, &str), String> {
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().unwrap_or("");
    if without_ending(first) != "---" {
        return Err("has no frontmatter (the file must start with a `---` line)".to_string());
    }
    let start = first.len();
    let mut end = start;
    for line in lines {
        if without_ending(line) == "---" {
            return Ok((&text[start..end], &text[end + line.len()..]));
        }
        end += line.len();
    }
    Err("its frontmatter has no closing `---` line".to_string())
}
