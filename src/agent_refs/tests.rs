use super::*;

/// The plan's worked example: header words around the reference stay
/// text, the reference is one segment whose words are the phrase.
#[test]
fn test_a_report_header_splits_at_its_reference() {
    assert_eq!(
        find("Report from agent #1234:\n\nAll done."),
        vec![
            Segment::Text("Report from "),
            Segment::Conversation { words: "agent #1234", id: 1234 },
            Segment::Text(":\n\nAll done."),
        ]
    );
}

/// Controls that stay one `Text`: no keyword, no `#`, no space, a joined
/// word, a leading zero, and a number too big for `i64`.
#[test]
fn test_controls_stay_text() {
    for text in [
        "#1234",
        "agent 1234",
        "agent#1234",
        "sub-agent #12",
        "subagent #12",
        "my_agent #12",
        "agent #0",
        "agent #007",
        // 25 digits: not 1–19 digits that parse as i64.
        "agent #1234567890123456789012345",
        // 19 digits but past i64::MAX.
        "conversation #9223372036854775808",
        // A word character right after the digits: not a reference.
        "agent #12a",
        "agent #12_3",
        // Nothing after the keyword but the space.
        "agent ",
        "",
    ] {
        assert_eq!(find(text), vec![Segment::Text(text)], "{text:?}");
    }
}

/// Forms that link: case-insensitive keywords, a reference set off by
/// punctuation, and digits ending at a non-word character (`'`) —
/// smelt's own `agent #123's start failed` strings link `agent #123`.
#[test]
fn test_keyword_forms_link() {
    assert_eq!(
        find("Agent #12"),
        vec![Segment::Conversation { words: "Agent #12", id: 12 }]
    );
    assert_eq!(
        find("(conversation #12)"),
        vec![
            Segment::Text("("),
            Segment::Conversation { words: "conversation #12", id: 12 },
            Segment::Text(")"),
        ]
    );
    assert_eq!(
        find("agent #123's"),
        vec![
            Segment::Conversation { words: "agent #123", id: 123 },
            Segment::Text("'s"),
        ]
    );
    // The header smelt actually writes: the persona suffix is left as text.
    assert_eq!(
        find("Report from agent #123 (smelt:x):"),
        vec![
            Segment::Text("Report from "),
            Segment::Conversation { words: "agent #123", id: 123 },
            Segment::Text(" (smelt:x):"),
        ]
    );
    // CONVERSATION #1 — the keyword is matched case-insensitively.
    assert_eq!(
        find("CONVERSATION #1"),
        vec![Segment::Conversation { words: "CONVERSATION #1", id: 1 }]
    );
}

/// Several references, and the whole number range the scan accepts.
#[test]
fn test_several_references_and_the_digit_range() {
    assert_eq!(
        find("agent #1 tells conversation #2 about agent #3"),
        vec![
            Segment::Conversation { words: "agent #1", id: 1 },
            Segment::Text(" tells "),
            Segment::Conversation { words: "conversation #2", id: 2 },
            Segment::Text(" about "),
            Segment::Conversation { words: "agent #3", id: 3 },
        ]
    );
    // i64::MAX itself (19 digits) still parses.
    assert_eq!(
        find("agent #9223372036854775807"),
        vec![Segment::Conversation {
            words: "agent #9223372036854775807",
            id: i64::MAX,
        }]
    );
}

/// Text at a non-space word boundary before the keyword — start of
/// text, punctuation, non-ASCII — still links; a letter, digit, `-` or
/// `_` glued in front does not.
#[test]
fn test_the_keyword_starts_at_a_word_boundary() {
    for prefix in ["", "(", "—", "see ", "\n", "x1 "] {
        let text = format!("{prefix}agent #7");
        // A reference at the very start carries no empty text segment.
        let expected: Vec<Segment<'_>> = if prefix.is_empty() {
            vec![Segment::Conversation { words: "agent #7", id: 7 }]
        } else {
            vec![Segment::Text(prefix), Segment::Conversation { words: "agent #7", id: 7 }]
        };
        assert_eq!(find(&text), expected, "{text:?}");
    }
    for prefix in ["x", "4", "-", "_", "sub"] {
        let text = format!("{prefix}agent #7");
        assert_eq!(find(&text), vec![Segment::Text(&text.clone())], "{text:?}");
    }
}
