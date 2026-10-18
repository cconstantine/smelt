//! `read_file`'s tests: unit tests on `read_file_response` with a built
//! `FileContents` — no pod, no database.
//!
//! The size assertions are against `MAX_TOOL_RESULT_CHARS`, the cap `execute`
//! puts on every tool result. Naming the number there instead guarded nothing
//! a lowered cap would have been caught by (SME-189 review 1, finding 3).

use super::*;
use crate::agent_protocol::FileContents;
use crate::anthropic::tools::server::MAX_TOOL_RESULT_CHARS;

/// A `read_file`'s page of a file, as the sandbox agent's reply hands it
/// over. `hash` is always the whole file's, so an `edit_file` after a
/// partial read checks against the whole file (SME-189).
fn contents(lines: Vec<String>, total_lines: usize) -> FileContents {
    FileContents { lines, total_lines, hash: "h".into() }
}

/// A `read_file_response` that is expected to succeed.
fn read(lines: Vec<String>, total_lines: usize, offset: u32, character: u32) -> Value {
    read_file_response(contents(lines, total_lines), offset, character).expect("the read succeeds")
}

/// A response's `content` with the `{:>6}\t` numbering taken off each line
/// and nothing else changed — so a cut line still wears its note.
fn shown_lines(response: &Value) -> Vec<String> {
    response["content"]
        .as_str()
        .expect("content")
        .lines()
        .map(|line| line.split_once('\t').expect("a numbered line").1.to_string())
        .collect()
}

/// A shown line split into its text and the `offset`/`character` its cut
/// note points at — `None` when it carries no note.
fn cut_note(shown: &str) -> (String, Option<(u32, u32)>) {
    let Some((text, note)) = shown.split_once("… [line ") else {
        return (shown.to_string(), None);
    };
    let after = note.split_once("read on with offset ").expect("the note says how to read on").1;
    let (offset, character) = after.split_once(", character ").expect("the note says which character");
    let pointer = (
        offset.parse::<u32>().expect("the note's offset is a number"),
        character.trim_end_matches(']').parse::<u32>().expect("the note's character is a number"),
    );
    (text.to_string(), Some(pointer))
}

/// The last `chars` characters of `line`, counted as characters — a failure
/// message never has to slice by bytes.
fn tail(line: &str, chars: usize) -> String {
    line.chars().skip(line.chars().count().saturating_sub(chars)).collect()
}

/// Follow a one-line file's notes from `character` to its end, joining what
/// each read shows. Returns the joined text and how many reads it took.
fn walk_a_line(line: &str, character: u32) -> (String, usize) {
    let mut joined = String::new();
    let mut character = character;
    let mut reads = 0;
    loop {
        let response = read(vec![line.to_string()], 1, 1, character);
        let page = shown_lines(&response);
        let (text, pointer) = cut_note(page.first().expect("the read shows its line"));
        joined.push_str(&text);
        reads += 1;
        let Some((offset, next)) = pointer else { break };
        assert_eq!(offset, 1, "a one-line file's note never points at another line");
        character = next;
    }
    (joined, reads)
}

/// SME-76: conversation 23 read a 73 KB source file whole.
#[test]
fn test_read_file_response_stops_at_the_budget_and_says_where_to_continue() {
    let mut lines = vec!["z".repeat(5_000)];
    lines.extend((0..60).map(|i| format!("{i:02}{}", "y".repeat(1_000))));
    let response = read(lines, 100, 11, 1);
    assert_eq!(response["truncated"], true);
    let content = response["content"].as_str().expect("content");
    let shown_count = content.lines().count() as u64;
    assert_eq!(response["next_offset"].as_u64(), Some(11 + shown_count), "{shown_count} lines shown");
    assert!(
        content.lines().next().expect("a line").ends_with(
            "[line 11 cut: characters 1–2000 of 5000 shown; read on with offset 11, character 2001]"
        ),
        "the cut line names itself and says how to read on: {}",
        tail(content.lines().next().expect("a line"), 120)
    );
    assert!(response.to_string().chars().count() < MAX_TOOL_RESULT_CHARS);
    assert_eq!(response["hash"], "h", "the hash is still the whole file's");
}

#[test]
fn test_read_file_response_gives_a_next_offset_when_the_file_goes_on() {
    let response = read(vec!["a".into(), "b".into()], 5, 1, 1);
    assert_eq!(response["next_offset"], 3);
    assert!(response.get("truncated").is_none(), "nothing was left out for size");
    let whole = read(vec!["a".into()], 1, 1, 1);
    assert!(whole.get("next_offset").is_none(), "the file ends here");
}

/// SME-189: a 40,000-character line came back cut at 2,000 with nothing
/// saying which line it was, how long it was, or how to go on.
#[test]
fn test_a_cut_line_names_itself_its_length_and_how_to_read_on() {
    let line = "x".repeat(40_000);
    let response = read(vec![line.clone()], 1, 1, 1);
    assert_eq!(response["total_lines"], 1);
    assert!(response.get("next_offset").is_none(), "a one-line file has no next whole line");
    assert!(response.get("truncated").is_none(), "no line was left out for the budget");
    assert!(response.get("character").is_none(), "a read from the line's start doesn't say so");
    let page = shown_lines(&response);
    assert_eq!(page.len(), 1);
    let (text, pointer) = cut_note(&page[0]);
    assert_eq!(text, line.chars().take(2_000).collect::<String>(), "the first 2,000 characters show");
    assert_eq!(pointer, Some((1, 2_001)), "the note points at the next character");
    assert!(
        page[0].ends_with("[line 1 cut: characters 1–2000 of 40000 shown; read on with offset 1, character 2001]"),
        "{}",
        tail(&page[0], 120)
    );
}

/// Reading on within a line starts where the note said, and says so itself.
#[test]
fn test_a_read_can_start_at_a_character_of_the_offset_line() {
    let line = "x".repeat(40_000);
    let response = read(vec![line.clone()], 1, 1, 2_001);
    assert_eq!(response["character"], 2_001, "the reply says this text isn't the line's start");
    assert!(response.get("next_offset").is_none(), "a one-line file has no next whole line");
    assert!(response.get("truncated").is_none(), "no line was left out for the budget");
    let page = shown_lines(&response);
    let (text, pointer) = cut_note(&page[0]);
    assert_eq!(text, line.chars().skip(2_000).take(2_000).collect::<String>(), "characters 2001–4000");
    assert_eq!(pointer, Some((1, 4_001)), "and on to the next 2,000");
    assert!(page[0].contains("characters 2001–4000 of 40000 shown"), "{}", tail(&page[0], 120));
}

/// Twenty reads, each starting where the last one's note pointed, cover a
/// 40,000-character line exactly.
#[test]
fn test_reading_on_walks_a_one_line_file_to_its_end() {
    let line: String = (0..40_000).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
    let (joined, reads) = walk_a_line(&line, 1);
    assert_eq!(reads, 20, "40,000 characters at 2,000 a read");
    assert_eq!(joined, line, "the reads join to the line exactly");
}

/// A read that starts mid-line is following one line's note, so it answers
/// that line and nothing else (SME-189 review 1, finding 1): the page after
/// it belongs to the next whole line, and re-sending it on every step of a
/// walk costs about twelve times what the walk wins.
#[test]
fn test_a_read_that_starts_mid_line_shows_only_that_line() {
    let long = "x".repeat(40_000);
    let lines: Vec<String> = std::iter::once(long.clone())
        .chain((1..=2_000).map(|i| format!("line {i} of a file that goes on for a while")))
        .collect();

    let response = read(lines.clone(), 2_001, 1, 2_001);
    let page = shown_lines(&response);
    assert_eq!(page.len(), 1, "a mid-line read answers the one line it started in");
    let (text, pointer) = cut_note(&page[0]);
    assert_eq!(text, long.chars().skip(2_000).take(2_000).collect::<String>(), "characters 2001–4000");
    assert_eq!(pointer, Some((1, 4_001)), "and on into the same line, not past it");
    assert_eq!(response["next_offset"], 2, "the next whole line, as ever");
    assert!(response.get("truncated").is_none(), "nothing was left out for the budget");

    // Following the notes from there to the end of the line costs the line,
    // not the file: nineteen reads of ~2,000 characters each, rather than
    // nineteen pages of everything that follows the long line.
    let mut joined = String::new();
    let mut sent = 0;
    let mut character = 2_001;
    let mut reads = 0;
    loop {
        let response = read(lines.clone(), 2_001, 1, character);
        let page = shown_lines(&response);
        assert_eq!(page.len(), 1, "read {} of the walk stayed on line 1", reads + 1);
        sent += response["content"].as_str().expect("content").chars().count();
        let (text, pointer) = cut_note(&page[0]);
        joined.push_str(&text);
        reads += 1;
        let Some((_, next)) = pointer else { break };
        character = next;
    }
    assert_eq!(reads, 19, "the remaining 38,000 characters at 2,000 a read");
    assert_eq!(joined, long.chars().skip(2_000).collect::<String>(), "the reads finish the line");
    assert!(sent < 45_000, "following the notes sent {sent} characters to read 38,000 of them");

    // A read from the line's start still pages on past it, as ever.
    let page = shown_lines(&read(lines, 2_001, 1, 1));
    assert!(page.len() > 1, "the read that found the long line keeps the lines after it");
}

/// A cut counts characters, never bytes, so it splits nothing — and a
/// grapheme made of several characters can still land in two reads, which is
/// fine because the reads join exactly.
#[test]
fn test_a_cut_never_splits_a_character() {
    // 1,999 `a`s, an emoji of four bytes, then 3,000 `é`s: 5,000 characters.
    let line = format!("{}😀{}", "a".repeat(1_999), "é".repeat(3_000));
    let response = read(vec![line.clone()], 1, 1, 1);
    let page = shown_lines(&response);
    let (text, pointer) = cut_note(&page[0]);
    assert_eq!(text.chars().count(), 2_000, "2,000 characters, however many bytes");
    assert!(text.ends_with('😀'), "the emoji is shown whole: {}", tail(&text, 12));
    assert_eq!(pointer, Some((1, 2_001)));
    assert!(page[0].contains("characters 1–2000 of 5000 shown"), "{}", tail(&page[0], 120));
    let (joined, reads) = walk_a_line(&line, 1);
    assert_eq!(reads, 3, "5,000 characters at 2,000 a read");
    assert_eq!(joined, line, "the reads join to the line exactly");

    // A decomposed `é` across the cut: the base letter ends one read, the
    // combining accent starts the next. Each read is valid text on its own.
    let decomposed = format!("{}e\u{301}{}", "a".repeat(1_999), "b".repeat(3_000));
    let response = read(vec![decomposed.clone()], 1, 1, 1);
    let page = shown_lines(&response);
    let (text, pointer) = cut_note(&page[0]);
    assert_eq!(pointer, Some((1, 2_001)));
    assert!(text.ends_with('e'), "the base letter ends this read: {}", tail(&text, 12));
    let next = shown_lines(&read(vec![decomposed.clone()], 1, 1, 2_001));
    assert!(
        next[0].starts_with('\u{301}'),
        "the accent is the next read's first character: {}",
        tail(&next[0], 12)
    );
    let (joined, _) = walk_a_line(&decomposed, 1);
    assert_eq!(joined, decomposed, "the reads join to the line exactly");
}

/// A long line in the middle doesn't cost the rest of the page.
#[test]
fn test_a_long_line_in_the_middle_leaves_the_rest_of_the_page_alone() {
    let long = "x".repeat(5_000);
    let response = read(vec!["foo".into(), long.clone(), "bar".into()], 3, 1, 1);
    let page = shown_lines(&response);
    assert_eq!(page.len(), 3, "all three lines show");
    assert_eq!(page[0], "foo");
    assert_eq!(page[2], "bar");
    let (text, pointer) = cut_note(&page[1]);
    assert_eq!(text, long.chars().take(2_000).collect::<String>());
    assert_eq!(pointer, Some((2, 2_001)), "the note names line 2, not line 1");
    assert!(
        page[1].contains("[line 2 cut: characters 1–2000 of 5000 shown; read on with offset 2, character 2001]"),
        "{}",
        tail(&page[1], 120)
    );
    assert!(response.get("next_offset").is_none(), "the file ends after line 3");
    assert!(response.get("truncated").is_none(), "nothing was left out for the budget");
}

/// `character` is a position in the line, so its far edge reads and one past
/// it is refused.
#[test]
fn test_a_character_past_the_end_of_its_line_is_refused() {
    let response = read(vec!["abcd".into()], 1, 1, 4);
    assert_eq!(shown_lines(&response), vec!["d".to_string()], "the last character, with no note");
    assert_eq!(response["character"], 4);

    let error = read_file_response(contents(vec!["abcd".into()], 1), 1, 5)
        .expect_err("one past the end of a 4-character line is a refusal, not an empty read");
    assert_eq!(error, "character 5 is past the end of line 1, which has 4 characters");
    let error = read_file_response(contents(vec!["abc".into()], 1), 1, 5).expect_err("likewise");
    assert_eq!(error, "character 5 is past the end of line 1, which has 3 characters");
    let error = read_file_response(contents(vec!["x".repeat(40_000)], 1), 1, 40_001).expect_err("likewise");
    assert_eq!(error, "character 40001 is past the end of line 1, which has 40000 characters");

    // A read that gets no line at all ignores `character`: an offset past
    // the end of the file is an empty read, as it has always been — and
    // claims no position in text it didn't show (SME-189 review 1, finding 5).
    let response = read(vec![], 3, 10, 5);
    assert_eq!(response["content"], "");
    assert!(response.get("character").is_none(), "an empty read has no position in it");
}

/// `character` is held to what a position can be before it is cast, so an
/// absurd number falls into the past-the-end refusal instead of wrapping
/// round to a real position and answering the wrong characters under a
/// position that looks self-consistent (SME-189 review 1, finding 2).
#[test]
fn test_a_character_too_big_for_a_position_is_refused_not_wrapped() {
    assert_eq!(character_param(&serde_json::json!({})), 1, "absent means the line's start");
    assert_eq!(character_param(&serde_json::json!({"character": 0})), 1, "0 means the line's start");
    assert_eq!(character_param(&serde_json::json!({"character": 2_001})), 2_001);
    // 2^32 and 2^32 + 2,000: cast as they were, they become 0 and 2,001 — so
    // one answers the line's start as if nothing were wrong, and the other
    // answers characters 2001–4000 labelled `"character": 2001`.
    assert_eq!(character_param(&serde_json::json!({"character": 4_294_967_296_u64})), u32::MAX);
    assert_eq!(character_param(&serde_json::json!({"character": 4_294_969_297_u64})), u32::MAX);

    let error = read_file_response(
        contents(vec!["abcd".into()], 1),
        1,
        character_param(&serde_json::json!({"character": 4_294_969_297_u64})),
    )
    .expect_err("an out-of-range position is refused, whatever its width");
    assert_eq!(error, "character 4294967295 is past the end of line 1, which has 4 characters");
}

/// The notes go on after `fit_lines`, so they cost nothing against the
/// budget — and a page of nothing but cut lines still fits under the cap.
#[test]
fn test_a_page_of_cut_lines_stays_under_the_tool_result_cap() {
    for (what, line) in [("plain", "x".repeat(2_500)), ("escaped", "\"".repeat(2_500))] {
        let response = read(vec![line; 2_000], 2_000, 1, 1);
        let json = response.to_string();
        let parsed: Value = serde_json::from_str(&json).expect("the response is still JSON");
        assert!(json.chars().count() < MAX_TOOL_RESULT_CHARS, "{what}: {} characters", json.chars().count());
        assert_eq!(parsed["truncated"], true, "{what}: some lines were left out");
        let page: Vec<&str> = parsed["content"].as_str().expect("content").lines().collect();
        assert!(
            page.len() >= 5 && page.len() <= 12,
            "{what}: {} cut lines fit the budget",
            page.len()
        );
        for line in &page {
            assert!(
                line.contains("; read on with offset "),
                "{what}: a kept line lost its way on: {}",
                tail(line, 60)
            );
        }
        assert_eq!(
            parsed["next_offset"].as_u64(),
            Some(1 + page.len() as u64),
            "{what}: the next read starts at the first line left out"
        );
    }
}
