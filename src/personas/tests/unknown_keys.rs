//! SME-506: `smelt` keys this smelt doesn't know — skipped only when the
//! file lists them in `smelt.ignore_if_unknown`, never when restricting,
//! and never inside a section.

use super::*;

/// A persona file whose `smelt` map is `smelt` (indented two spaces).
fn file(smelt: &str) -> String {
    format!("---\nname: developer\ndescription: d\nsmelt:\n{smelt}---\nbody\n")
}

// --- T1: a listed unknown key is skipped ---

#[test]
fn test_a_listed_unknown_key_is_skipped_and_named_wherever_the_list_is() {
    for smelt in [
        "  model: m\n  compact_later: {any: [1]}\n  ignore_if_unknown: [compact_later]\n",
        "  ignore_if_unknown: [compact_later]\n  model: m\n  compact_later: {any: [1]}\n",
    ] {
        let persona = parse_ok(DEV, &file(smelt));
        assert_eq!(persona.ignored, vec!["compact_later".to_string()], "{smelt}");
        assert_eq!(persona.model.as_deref(), Some("m"), "a known key beside it is kept: {smelt}");
    }
    // Several, in file order; a listed key that isn't there is no matter.
    let persona = parse_ok(DEV, &file("  zeta: 1\n  alpha: [x]\n  ignore_if_unknown: [alpha, zeta, absent]\n"));
    assert_eq!(persona.ignored, vec!["zeta".to_string(), "alpha".to_string()]);
    // No list, no unknown key: nothing ignored.
    assert!(parse_ok(DEV, &file("  model: m\n")).ignored.is_empty());
}

// --- T2: an unlisted unknown key is still refused, with the hint ---

#[test]
fn test_an_unlisted_unknown_key_is_refused_at_its_line_naming_the_list() {
    let error = parse_err(DEV, &file("  foo: 1\n  ignore_if_unknown: [bar]\n"));
    assert!(error.contains("smelt: unknown field `foo`"), "{error}");
    assert!(error.contains("line 5"), "the key is on the file's line 5: {error}");
    assert!(error.contains("smelt.ignore_if_unknown"), "the refusal names the way out: {error}");
}

// --- T3: a restricting key can never be listed ---

#[test]
fn test_a_restricting_key_in_the_list_is_refused_present_or_not() {
    for name in ["tools", "credentials", "resources", "egress", "limits"] {
        for smelt in [format!("  ignore_if_unknown: [{name}]\n"), format!("  {name}: ~\n  ignore_if_unknown: [{name}]\n")] {
            let error = parse_err(DEV, &file(&smelt));
            assert!(
                error.contains(&format!("`smelt.{name}` narrows what the persona may do")),
                "{name}: {smelt}: {error}"
            );
            assert!(error.contains("line "), "{error}");
        }
    }
}

// --- T4: sections stay strict ---

#[test]
fn test_an_unknown_key_inside_a_section_is_refused_even_when_listed() {
    let cases = [
        ("  tools:\n    class: read-only\n    alow: [x]\n  ignore_if_unknown: [alow]\n", "alow", "line 7"),
        ("  tools:\n    own: {gpu: 1}\n", "gpu", "line 6"),
        ("  resources:\n    sandbox: {gpu: 1}\n  ignore_if_unknown: [gpu]\n", "gpu", "line 6"),
    ];
    for (smelt, key, line) in cases {
        let error = parse_err(DEV, &file(smelt));
        assert!(error.contains(&format!("unknown field `{key}`")) && error.contains(line), "{smelt}: {error}");
    }
    let error = parse_err(DEV, &file("  ignore_if_unknown: [tools.alow]\n"));
    assert!(error.contains("tools.alow") && error.contains("isn't a smelt key's name"), "{error}");
}

// --- T5: a known key is always parsed, listed or not ---

#[test]
fn test_a_known_key_with_a_bad_value_is_refused_even_when_listed() {
    let error = parse_err(DEV, &file("  compact_at: 0\n  ignore_if_unknown: [compact_at]\n"));
    assert!(error.contains("invalid value") && error.contains("compact_at") && error.contains("line 5"), "{error}");
    let error = parse_err(DEV, &file("  model: \"\"\n  ignore_if_unknown: [model]\n"));
    assert!(error.contains("smelt.model"), "{error}");
    let error = parse_err(DEV, &file("  tools: {class: bogus}\n"));
    assert!(error.contains("bogus"), "{error}");
    // The value is read first, where the key stands, so its own error wins.
    let error = parse_err(DEV, &file("  credentials: [\"a b\"]\n  ignore_if_unknown: [credentials]\n"));
    assert!(error.contains("`a b` isn't a credential name"), "{error}");
    let error = parse_err(DEV, &file("  resources:\n    sandbox: { memory_request: 2Gi, memory_limit: 1Gi }\n"));
    assert!(error.contains("smelt.resources.sandbox.memory_request"), "{error}");
}

// --- T6: a malformed list or name is refused at its line ---

#[test]
fn test_a_malformed_ignore_list_or_name_is_refused_at_its_line() {
    let long = "a".repeat(65);
    for list in ["compact_later", "[1]", "[Foo]", "[\"a b\"]", &format!("[{long}]")] {
        let error = parse_err(DEV, &file(&format!("  ignore_if_unknown: {list}\n")));
        assert!(error.contains("ignore_if_unknown") || error.contains("invalid type"), "{list}: {error}");
        assert!(error.contains("line 5"), "{list}: {error}");
        assert!(!error.contains("unknown field `ignore_if_unknown`"), "the list is a known key: {list}: {error}");
    }
    // The name's shape decides before the list does.
    let error = parse_err(DEV, &file("  Foo_Bar: 1\n  ignore_if_unknown: [Foo_Bar]\n"));
    assert!(error.contains("Foo_Bar") && error.contains("line 5"), "{error}");
    // The longest name is fine.
    let longest = format!("a{}", "b".repeat(63));
    let persona = parse_ok(DEV, &file(&format!("  {longest}: 1\n  ignore_if_unknown: [{longest}]\n")));
    assert_eq!(persona.ignored, vec![longest]);
    // No value is an empty list, like credentials.
    assert!(parse_ok(DEV, &file("  ignore_if_unknown:\n")).ignored.is_empty());
}

// --- T7: the table is the parser's ---

#[test]
fn test_every_accepted_key_parses_and_the_refusal_lists_exactly_them() {
    use settings::{Kind, KEYS};
    for key in KEYS.iter().filter(|k| k.kind != Kind::Reserved) {
        parse_ok(DEV, &file(&format!("  {}: ~\n", key.name)));
    }
    let error = parse_err(DEV, &file("  nope: 1\n"));
    let listed = error.split("expected one of ").nth(1).expect("the names").split(" (").next().expect("the list");
    let names: Vec<&str> = KEYS.iter().filter(|k| k.kind != Kind::Reserved).map(|k| k.name).collect();
    assert_eq!(listed, names.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", "), "{error}");
    for reserved in ["egress", "limits"] {
        assert!(!names.contains(&reserved), "{reserved} is reserved, not accepted");
        assert!(parse_err(DEV, &file(&format!("  {reserved}: []\n"))).contains("unknown field"));
    }
}
