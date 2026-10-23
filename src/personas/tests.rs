use super::*;

const DEV: &str = ".agents/personas/developer.md";

fn bytes(text: &str) -> u64 {
    text.len() as u64
}

fn parse_ok(path: &str, text: &str) -> Persona {
    parse(path, text, bytes(text)).unwrap_or_else(|e| panic!("{path} should parse: {e}"))
}

fn parse_err(path: &str, text: &str) -> String {
    let error = parse(path, text, bytes(text)).expect_err("should be refused");
    assert!(error.starts_with(path), "an error names the file first: {error}");
    error
}

// --- T1: good files ---

#[test]
fn test_a_full_persona_file_parses_with_its_body_as_written() {
    let text = "---\nname: developer\ndescription: Works one Todo ticket through to In Review.\nsmelt:\n  model: claude-opus-4-5\n---\n\
                Take on the Developer persona: read [developer.md](../../docs/personas/developer.md).\n";
    assert_eq!(
        parse_ok(DEV, text),
        Persona {
            name: "developer".to_string(),
            description: "Works one Todo ticket through to In Review.".to_string(),
            instructions: "Take on the Developer persona: read [developer.md](../../docs/personas/developer.md).\n"
                .to_string(),
            model: Some("claude-opus-4-5".to_string()),
        }
    );
}

#[test]
fn test_the_smelt_key_is_optional_and_may_be_empty() {
    for text in [
        "---\nname: developer\ndescription: d\n---\nbody\n",
        "---\nname: developer\ndescription: d\nsmelt:\n---\nbody\n",
        "---\nname: developer\ndescription: d\nsmelt: {}\n---\nbody\n",
    ] {
        let persona = parse_ok(DEV, text);
        assert_eq!((persona.model, persona.instructions.as_str()), (None, "body\n"), "{text}");
    }
}

/// YAML 1.2's core schema: `no` is a string, not a boolean.
#[test]
fn test_a_description_of_no_stays_the_string_no() {
    assert_eq!(parse_ok(DEV, "---\nname: developer\ndescription: no\n---\n").description, "no");
}

#[test]
fn test_a_crlf_persona_file_parses() {
    let persona = parse_ok(DEV, "---\r\nname: developer\r\ndescription: d\r\n---\r\nbody\r\n");
    assert_eq!((persona.name.as_str(), persona.instructions.as_str()), ("developer", "body\r\n"));
}

// --- T2: unknown fields ---

#[test]
fn test_an_unknown_top_level_field_is_refused_naming_it_and_its_line() {
    // Claude Code's own `tools` and `model` included: smelt's go under `smelt`.
    for field in ["tools", "model", "disallowedTools"] {
        let text = format!("---\nname: developer\ndescription: d\n{field}: x\n---\n");
        let error = parse_err(DEV, &text);
        assert!(error.contains(field), "{error}");
        assert!(error.contains("line 4"), "the key is on the file's line 4: {error}");
        assert!(!error.contains("line 3"), "serde's own line isn't the file's: {error}");
    }
}

#[test]
fn test_an_unknown_field_under_smelt_is_refused_naming_it() {
    let text = "---\nname: developer\ndescription: d\nsmelt:\n  tools:\n    class: read-only\n---\n";
    let error = parse_err(DEV, text);
    assert!(error.contains("smelt: unknown field `tools`"), "{error}");
    assert!(error.contains("line 5"), "the key is on the file's line 5: {error}");
}

// --- T3: missing or bad ---

#[test]
fn test_a_missing_name_or_description_is_refused_naming_it() {
    let error = parse_err(DEV, "---\ndescription: d\n---\n");
    assert!(error.contains("name"), "{error}");
    let error = parse_err(DEV, "---\nname: developer\n---\n");
    assert!(error.contains("description"), "{error}");
}

#[test]
fn test_the_name_must_be_the_file_name() {
    let error = parse_err(DEV, "---\nname: dev\ndescription: d\n---\n");
    assert!(error.contains("`dev`") && error.contains("developer.md"), "{error}");
}

#[test]
fn test_a_name_outside_lowercase_letters_digits_and_dashes_is_refused() {
    for name in ["code_reviewer", "-dev", "dev-", "Dev", ""] {
        let path = format!(".agents/personas/{name}.md");
        let error = parse_err(&path, &format!("---\nname: \"{name}\"\ndescription: d\n---\n"));
        assert!(error.contains("name"), "{name}: {error}");
    }
    let long = "a".repeat(65);
    let path = format!(".agents/personas/{long}.md");
    parse_err(&path, &format!("---\nname: {long}\ndescription: d\n---\n"));
    let longest = "a".repeat(64);
    parse_ok(&format!(".agents/personas/{longest}.md"), &format!("---\nname: {longest}\ndescription: d\n---\n"));
    parse_ok(".agents/personas/code-reviewer-2.md", "---\nname: code-reviewer-2\ndescription: d\n---\n");
}

#[test]
fn test_a_description_must_be_1_to_1024_characters() {
    let error = parse_err(DEV, "---\nname: developer\ndescription: \"\"\n---\n");
    assert!(error.contains("description"), "{error}");
    let error = parse_err(DEV, &format!("---\nname: developer\ndescription: {}\n---\n", "é".repeat(1025)));
    assert!(error.contains("description"), "{error}");
    // Characters, not bytes: 1,024 two-byte ones are fine.
    parse_ok(DEV, &format!("---\nname: developer\ndescription: {}\n---\n", "é".repeat(1024)));
}

#[test]
fn test_an_empty_model_is_refused() {
    let error = parse_err(DEV, "---\nname: developer\ndescription: d\nsmelt:\n  model: \"\"\n---\n");
    assert!(error.contains("model"), "{error}");
}

#[test]
fn test_a_file_without_frontmatter_is_refused() {
    let error = parse_err(DEV, "# Developer\n\nname: developer\n");
    assert!(error.contains("no frontmatter"), "{error}");
    let error = parse_err(DEV, "---\nname: developer\ndescription: d\n");
    assert!(error.contains("no closing `---`"), "{error}");
}

#[test]
fn test_a_file_over_32_kib_is_refused_not_cut() {
    let text = "---\nname: developer\ndescription: d\n---\nbody\n";
    let error = parse(DEV, text, 32 * 1024 + 1).expect_err("too big");
    assert!(error.starts_with(DEV) && error.contains("32768"), "{error}");
    parse(DEV, text, 32 * 1024).expect("exactly 32 KiB is fine");
}

#[test]
fn test_a_duplicate_key_or_a_frontmatter_that_isnt_a_mapping_is_refused() {
    let error = parse_err(DEV, "---\nname: developer\nname: developer\ndescription: d\n---\n");
    assert!(error.contains("name"), "{error}");
    parse_err(DEV, "---\n- name\n- description\n---\n");
    parse_err(DEV, "---\n---\nbody\n");
    parse_err(DEV, "---\nname: [developer]\ndescription: d\n---\n");
}

// --- the split, on its own (SME-37's skills will call it) ---

#[test]
fn test_split_returns_the_frontmatter_and_the_body_after_its_closing_line() {
    assert_eq!(
        frontmatter::split("---\na: 1\n---\nbody\n---\nstill body\n"),
        Ok(("a: 1\n", "body\n---\nstill body\n"))
    );
    assert_eq!(frontmatter::split("---\na: 1\n---"), Ok(("a: 1\n", "")));
    assert_eq!(frontmatter::split("---\n---\n"), Ok(("", "")));
    // A line that only starts with --- isn't the closing line.
    assert!(frontmatter::split("---\na: 1\n----\n").is_err());
    assert!(frontmatter::split(" ---\na: 1\n---\n").is_err());
    assert!(frontmatter::split("").is_err());
}
