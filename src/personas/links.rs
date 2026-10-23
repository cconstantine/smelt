//! A persona's relative links made absolute (SME-236): a persona links
//! the repo's other docs rather than copying them, relative to its own
//! file, and the child reads them from its checkout with `read_file`,
//! which takes absolute paths (docs/design/personas/persona-files.md).

use pulldown_cmark::{Event, LinkType, Options, Parser, Tag};
use std::ops::Range;

/// `body` with each relative link and image destination (inline, or in a
/// reference definition) replaced by its absolute path under `dir`, the
/// persona file's directory in the child's checkout
/// (`/workspace/<dir>/.agents/personas`): `.` and `..` resolved, a
/// `#fragment` kept. A destination with a scheme, or starting with `/` or
/// `#`, and one that would resolve above the checkout
/// (`/workspace/<dir>`), is left as written; so is everything else in the
/// body, byte for byte. Links inside code are not links.
pub fn absolute(body: &str, dir: &str) -> String {
    let checkout = checkout_of(dir);
    let parser = Parser::new_ext(body, Options::empty());
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let mut offsets = parser.into_offset_iter();
    for (event, range) in offsets.by_ref() {
        if let Event::Start(Tag::Link { link_type, dest_url, .. } | Tag::Image { link_type, dest_url, .. }) = event
            && link_type == LinkType::Inline
            && let Some(at) = destination_in(body, range, "](", &dest_url)
            && let Some(resolved) = resolve(&dest_url, dir, &checkout)
        {
            edits.push((at, resolved));
        }
    }
    for (_, def) in offsets.reference_definitions().iter() {
        if let Some(at) = destination_in(body, def.span.clone(), "]:", &def.dest)
            && let Some(resolved) = resolve(&def.dest, dir, &checkout)
        {
            edits.push((at, resolved));
        }
    }
    edits.sort_by_key(|(at, _)| at.start);
    let mut out = String::with_capacity(body.len());
    let mut last = 0;
    for (at, resolved) in edits {
        if at.start < last {
            continue;
        }
        out.push_str(&body[last..at.start]);
        out.push_str(&resolved);
        last = at.end;
    }
    out.push_str(&body[last..]);
    out
}

/// `/workspace/<dir>` of a directory under it.
fn checkout_of(dir: &str) -> String {
    let parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).take(2).collect();
    format!("/{}", parts.join("/"))
}

/// Where `dest` is written in `body[span]`: right after `opener` (`](` for
/// an inline link, `]:` for a definition), past spaces and an opening `<`.
/// `None` when it isn't written there as given (an escape in it, say).
fn destination_in(body: &str, span: Range<usize>, opener: &str, dest: &str) -> Option<Range<usize>> {
    if dest.is_empty() {
        return None;
    }
    let source = body.get(span.clone())?;
    let mut from = 0;
    while let Some(found) = source[from..].find(opener) {
        let after = from + found + opener.len();
        let rest = &source[after..];
        let skipped = rest.len() - rest.trim_start().len();
        let rest = rest.trim_start();
        let (bracket, rest) = match rest.strip_prefix('<') {
            Some(inner) => (1, inner),
            None => (0, rest),
        };
        if rest.starts_with(dest) {
            let start = span.start + after + skipped + bracket;
            return Some(start..start + dest.len());
        }
        from = after;
    }
    None
}

/// `dest` made absolute under `dir`, or `None` to leave it as written.
fn resolve(dest: &str, dir: &str, checkout: &str) -> Option<String> {
    if dest.starts_with('/') || dest.starts_with('#') || has_scheme(dest) {
        return None;
    }
    let split = dest.find(['#', '?']).unwrap_or(dest.len());
    let (path, suffix) = dest.split_at(split);
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    let floor = checkout.split('/').filter(|p| !p.is_empty()).count();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.len() <= floor {
                    return None;
                }
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    let trailing = if path.ends_with('/') { "/" } else { "" };
    Some(format!("/{}{trailing}{suffix}", parts.join("/")))
}

/// Whether `dest` starts with a URL scheme (`https:`, `mailto:`).
fn has_scheme(dest: &str) -> bool {
    match dest.find(':') {
        Some(colon) if colon > 0 => {
            let scheme = &dest[..colon];
            scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        }
        _ => false,
    }
}
