//! The plain-text rule (SME-276): which characters can break or reorder a
//! line, or render as nothing so a reader can't see them, and the escaping
//! that shows them for what they are.
//!
//! Ungated, unlike the `server` module: the server applies it before text
//! reaches the model, and since SME-277 the chat frontend applies it when
//! showing a user a file they're being asked to read — the same rule on
//! both sides of the boundary, one copy.

/// What `plain_path_text` refuses and `plain_multiline_text` escapes: a
/// character that can break or reorder a line, or one that hides text.
pub fn not_plain(c: char) -> bool {
    breaks_a_line(c) || hides_text(c)
}

/// A control character (C0, DEL, C1), line or paragraph separator, or
/// bidi control.
fn breaks_a_line(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{2028}' | '\u{2029}' | '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

/// Unicode's `Default_Ignorable_Code_Point` (DerivedCoreProperties
/// 18.0.0), the characters that render as nothing by default (SME-276):
/// a person reading the text can't see them, while the model reads them,
/// so tag characters (U+E0000–E007F) can carry hidden ASCII and variation
/// selectors hidden bytes. Zero-width spaces and joiners, the soft
/// hyphen, the BOM, Hangul fillers and the reserved ignorables too. It
/// overlaps `breaks_a_line` on the bidi controls.
fn hides_text(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{034f}'
            | '\u{061c}'
            | '\u{115f}'..='\u{1160}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180b}'..='\u{180f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{3164}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{feff}'
            | '\u{ffa0}'
            | '\u{fff0}'..='\u{fff8}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0000}'..='\u{e0fff}'
    )
}

/// `text` (a failed clone's error, which the remote can write into)
/// made safe to show the model (SME-275): every character
/// `plain_path_text` refuses, except the newlines and tabs git's own
/// messages use, is written as its `\u{…}` escape, the ones that render
/// as nothing included (SME-276). The escape is ASCII, shows the model
/// what was there, and leaves already-escaped text as it is, so escaping
/// twice changes nothing.
pub fn plain_multiline_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if not_plain(c) && c != '\n' && c != '\t' {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}
