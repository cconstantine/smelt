//! References to another conversation written as `agent #N` or
//! `conversation #N` (SME-384): the one pure scan that finds them.
//!
//! The writer's words are never rewritten — nothing here touches a
//! stored string — but two renderers want to know where a reference is
//! so they can show it as a link to `/conversation/N`:
//! `markdown::parse` for assistant replies (whose autolink walk already
//! skips code and existing links), and `transcript`'s `text_with_refs`
//! for the plain paths smelt's own report headers and notices take.
//! Server render, client hydration and the streaming bubble all run this
//! same function over the same text, so they render the same DOM; that
//! purity is why there is no existence check — the render path reads no
//! database, and a link to a deleted id lands on the chat's
//! "doesn't exist" state instead.

#[cfg(test)]
mod tests;

/// A run of text split at conversation references.
#[derive(Debug, Clone, PartialEq)]
pub enum Segment<'a> {
    /// Words with no reference in them.
    Text(&'a str),
    /// A reference to conversation `id`. `words` is the matched phrase
    /// (`agent #1234`, as written) and is what the link shows.
    Conversation { words: &'a str, id: i64 },
}

/// The keywords that make a `#N` a conversation reference.
const KEYWORDS: [&str; 2] = ["conversation", "agent"];

/// Splits `text` into text and `agent #N` / `conversation #N`
/// references: a single `Text` segment when it holds none.
///
/// A reference is, exactly: a keyword from [`KEYWORDS`] (case-
/// insensitive) starting at a word boundary (text start, or after
/// anything but an alphanumeric, `-` or `_`), one ASCII space, `#`, and
/// 1–19 digits that don't start with `0` and parse as `i64`, with a non-
/// word character or end of text after them. So `subagent #12` and
/// `my_agent #12` stay text, `agent #123's start failed` links
/// `agent #123`, and the header `Report from agent #123 (smelt:x):`
/// links `agent #123` and leaves ` (smelt:x):` as text.
pub fn find(text: &str) -> Vec<Segment<'_>> {
    let bytes = text.as_bytes();
    let mut out: Vec<Segment<'_>> = Vec::new();
    let mut rest = 0usize; // start of the not-yet-pushed text run
    let mut at = 0usize; // scan position, a byte index into text
    while let Some(seek) = bytes[at..].iter().position(|&b| b == b'#') {
        let hash = at + seek;
        at = hash + 1;
        let Some(keyword) = KEYWORDS.iter().find(|k| keyword_before(text, hash, k)) else {
            continue;
        };
        let mut end = hash + 1;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        let digits = &text[hash + 1..end];
        if digits.is_empty() || digits.len() > 19 || digits.starts_with('0') {
            continue;
        }
        let Ok(id) = digits.parse::<i64>() else { continue };
        // A word character right after the digits means it isn't a
        // number on its own ("agent #12a").
        if text[end..].chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let start = hash - keyword.len() - 1;
        if start > rest {
            out.push(Segment::Text(&text[rest..start]));
        }
        out.push(Segment::Conversation { words: &text[start..end], id });
        rest = end;
    }
    if out.is_empty() {
        out.push(Segment::Text(text));
    } else if rest < text.len() {
        out.push(Segment::Text(&text[rest..]));
    }
    out
}

/// Whether the word `keyword` (compared case-insensitively) plus exactly
/// one ASCII space ends just before the `#` at `hash`, and starts at a
/// word boundary: text start, or preceded by anything but an
/// alphanumeric, `-` or `_`.
fn keyword_before(text: &str, hash: usize, keyword: &str) -> bool {
    let Some(space) = hash.checked_sub(1) else { return false };
    if text.as_bytes().get(space) != Some(&b' ') {
        return false;
    }
    let Some(start) = space.checked_sub(keyword.len()) else { return false };
    if text.get(start..space).is_none_or(|word| !word.eq_ignore_ascii_case(keyword)) {
        return false;
    }
    match text[..start].chars().next_back() {
        None => true,
        Some(c) => !(c.is_alphanumeric() || c == '-' || c == '_'),
    }
}
