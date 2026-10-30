//! The `/secrets` API (SME-242, T10 and T14): what leaves here is a
//! promise — a value goes in and nothing like it comes out beyond D7's
//! capped `shown` — and the rules are said with one voice whether the
//! form caught them or this code did.

use super::*;
use sqlx::PgPool;

fn marker() -> &'static str {
    "MARKER-7-not-a-real-secret-value"
}

/// T10: add, list, change, delete — with the name, dates and `shown`
/// crossing and the value never doing so, in a response body or in an
/// error message, in any sequence.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_value_goes_in_and_never_comes_out(pool: PgPool) {
    let pool = &pool;
    assert!(list_secrets_of(pool).await.expect("empty list").is_empty());

    let saved = save_secret_of(pool, None, "linear-key", marker()).await.expect("saved");
    let listed = list_secrets_of(pool).await.expect("listed");
    assert_eq!(listed.len(), 1);
    let row = &listed[0];
    assert_eq!(row.id, saved.id);
    assert_eq!(row.name, "linear-key");
    assert_eq!(row.updated_at, row.created_at);
    // The display D7 allows, and nothing else of the value.
    assert_eq!(row.shown, "MARK… (32 bytes)");

    // The response body, not just the struct: neither the whole value
    // nor more than the four characters D7 shows.
    let body = serde_json::to_string(&listed).expect("json");
    assert!(body.contains("MARK"), "the allowed prefix shows: {body}");
    for leaked in [marker(), "MARKE"] {
        assert!(!body.contains(leaked), "{leaked} reached the list body: {body}");
    }

    // Changing the value leaves the name alone and moves `shown`.
    let changed = save_secret_of(pool, Some(saved.id), "ignored", "shorter-42").await.expect("changed");
    assert_eq!(changed.name, "linear-key", "the name is immutable");
    assert_eq!(changed.shown, "sh… (10 bytes)");
    let body = serde_json::to_string(&list_secrets_of(pool).await.expect("listed")).expect("json");
    assert!(!body.contains("shorter-42") && !body.contains("shor"), "{body}");

    // A failed save says the rule, never the value it was handed.
    let err = save_secret_of(pool, None, "bad name!", marker()).await.expect_err("bad name");
    assert!(err.contains("1 to 64 letters"), "{err}");
    assert!(!err.contains("MARKER") && !err.contains("MARKE"), "{err} quotes the value");
    let err = save_secret_of(pool, None, "x", "").await.expect_err("empty value");
    assert!(err.contains("can't be empty"), "{err}");
    // `git-ssh` is the built-in's name: refused with its own words.
    let err = save_secret_of(pool, None, "git-ssh", "v").await.expect_err("the built-in's name");
    assert!(err.contains("built-in git key"), "{err}");
    assert!(list_secrets_of(pool).await.expect("listed").iter().any(|s| s.name == "linear-key"));

    // A name twice is one secret: the second submit is refused, its
    // value nowhere.
    let err = save_secret_of(pool, None, "linear-key", marker()).await.expect_err("unique name");
    assert!(err.contains("already stored"), "{err}");
    assert!(!err.contains("MARKER"), "{err} quotes the value");

    // Change what isn't there: the row says so.
    let err = save_secret_of(pool, Some(999_999), "n", "v").await.expect_err("no row");
    assert!(err.contains("No secret with that id"), "{err}");

    // Delete takes the row (and runs the live-pod install beside it, the
    // path the real-cluster tests exercise). Deleting what another tab
    // already deleted is that, not an error to raise.
    delete_secret_of(pool, saved.id).await.expect("deleted");
    assert!(list_secrets_of(pool).await.expect("listed").is_empty());
    delete_secret_of(pool, saved.id).await.expect("already gone");
}

/// T14: D7's arithmetic, exactly. Never more than four characters, at
/// least eight bytes of every value always absent — asserted on the
/// value's tail, not on counts, so a helper stubbed as the whole value
/// names itself here.
#[test]
fn test_shown_shows_at_most_four_characters_and_hides_eight_bytes() {
    // Too short to show anything of it: 6 bytes, no characters.
    assert_eq!(shown("abcdef"), "(6 bytes)");
    // The boundary: exactly 8 bytes hides all 8.
    assert_eq!(shown("01234567"), "(8 bytes)");
    // 11 bytes: its first 3 and the length, never the 4th.
    assert_eq!(shown("0123456789a"), "012… (11 bytes)");
    // 12: the cap of four, never the 5th.
    let v40 = "abcdefghijklmnopqrstuvwxyz01234567895678";
    assert_eq!(shown(v40), "abcd… (40 bytes)");
    // A non-ASCII lead, counted in bytes: taking characters here could
    // display four bytes for every character counted and leave the
    // eight hidden bytes shown. `k` bytes are shown, never a partial
    // character — so 16 bytes of four-byte characters show one
    // character, not four.
    let four_by_four = "😀😀😀😀"; // 16 bytes, 4 chars
    assert_eq!(shown(four_by_four), "😀… (16 bytes)");
    // Two-byte characters: `k` = 4 bytes lands on a boundary, so two
    // characters show and 8 of the 12 bytes stay hidden.
    let six_two = "éééééé"; // 12 bytes, 6 chars
    assert_eq!(shown(six_two), "éé… (12 bytes)");
    // A 3-byte character lead: `k` = 3 lands on its boundary and one
    // character shows; a longer string whose `k` would cut a character
    // trims back to the boundary instead of showing half of one.
    let lead3 = format!("日{a}", a = "abcdefgh"); // 3 + 8 = 11 bytes, k = 3
    assert_eq!(shown(&lead3), "日… (11 bytes)");
    let cut_mid = format!("{s}{t}", s = "日本語", t = "abcd"); // 9 + 4 = 13 bytes, k = 4 cuts at 1
    assert_eq!(shown(&cut_mid), "日… (13 bytes)");
    for (value, head_len) in [("0123456789a", 3), (v40, 4), (four_by_four, 1), (six_two, 2), (lead3.as_str(), 1)] {
        let head: String = value.chars().take(head_len + 1).collect();
        assert!(!shown(value).contains(&head), "{head} reached the display of a {value:?}");
        let tail_start = value.len().saturating_sub(8);
        assert!(!shown(value).contains(&value[tail_start..]), "the last 8 bytes reached the display");
    }
    // The length is exact (it's what makes rows tell apart).
    assert!(shown(v40).contains("(40 bytes)"));
}
