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
            tools: None,
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
    // `tools` is a real field now (SME-241); `credentials` is the next
    // reserved key (SME-242), refused until its ticket lands.
    let text = "---\nname: developer\ndescription: d\nsmelt:\n  credentials:\n    - git-ssh\n---\n";
    let error = parse_err(DEV, text);
    assert!(error.contains("smelt: unknown field `credentials`"), "{error}");
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

// --- SME-236: relative links made absolute when a persona is started ---

const DIR: &str = "/workspace/smelt/.agents/personas";

#[test]
fn test_links_inline_reference_and_image_become_absolute() {
    let body = "See [the rules](../../docs/rules.md) and [plan][p].\n\n![diagram](img/flow.png \"Flow\")\n\n[p]: ./plan.md \"The plan\"\n";
    assert_eq!(
        links::absolute(body, DIR),
        "See [the rules](/workspace/smelt/docs/rules.md) and [plan][p].\n\n![diagram](/workspace/smelt/.agents/personas/img/flow.png \"Flow\")\n\n[p]: /workspace/smelt/.agents/personas/plan.md \"The plan\"\n"
    );
}

#[test]
fn test_links_keep_a_fragment_and_angle_brackets() {
    let body = "[a](../../docs/process.md#gates) and [b](<my notes.md>) and [c](x.md?raw=1)";
    assert_eq!(
        links::absolute(body, DIR),
        "[a](/workspace/smelt/docs/process.md#gates) and [b](</workspace/smelt/.agents/personas/my notes.md>) and [c](/workspace/smelt/.agents/personas/x.md?raw=1)"
    );
}

#[test]
fn test_links_left_alone_absolute_urls_fragments_escapes_and_code() {
    let body = "[web](https://example.com/x.md) [mail](mailto:a@b.c) [top](#intro) [abs](/etc/hosts) \
                [out](../../../other/x.md) `[code](x.md)`\n\n```\n[block](y.md)\n```\n";
    assert_eq!(links::absolute(body, DIR), body, "nothing here is a relative link inside the checkout");
}

// --- SME-241: the smelt.tools grammar ---

#[test]
fn test_smelt_tools_parses_class_allow_and_deny() {
    let text = "---\nname: developer\ndescription: d\nsmelt:\n  tools:\n    class: read-only\n    \
                allow: [github/create_pull_request_review, write_file, browser_*]\n    \
                deny: [github/merge_pull_request]\n---\n";
    let rules = parse_ok(DEV, text).tools.expect("the parsed rules");
    assert_eq!(rules.class, rules::Ceiling::ReadOnly);
    assert_eq!(
        rules.allow,
        vec![
            rules::Pattern { server: Some("github".to_string()), tool: "create_pull_request_review".to_string() },
            rules::Pattern { server: None, tool: "write_file".to_string() },
            rules::Pattern { server: None, tool: "browser_*".to_string() },
        ],
        "an MCP pattern splits at its one /, a smelt one doesn't"
    );
    assert_eq!(rules.deny, vec![rules::Pattern { server: Some("github".to_string()), tool: "merge_pull_request".to_string() }]);
}

#[test]
fn test_the_tools_defaults_are_the_all_ceiling_and_no_patterns() {
    let empty = parse_ok(DEV, "---\nname: developer\ndescription: d\nsmelt:\n  tools: {}\n---\n")
        .tools
        .expect("tools, even empty");
    assert_eq!(empty.class, rules::Ceiling::All, "`all` is the default ceiling");
    assert!(empty.allow.is_empty() && empty.deny.is_empty(), "no patterns unless the file writes them");
    let named = parse_ok(DEV, "---\nname: developer\ndescription: d\nsmelt:\n  tools:\n    class: no-destructive\n---\n")
        .tools
        .expect("tools");
    assert_eq!(named.class, rules::Ceiling::NoDestructive);
}

#[test]
fn test_an_unknown_tools_class_is_refused_listing_the_three() {
    let error = parse_err(DEV, "---\nname: developer\ndescription: d\nsmelt:\n  tools:\n    class: readonly\n---\n");
    assert!(error.contains("readonly"), "{error}");
    for spelling in ["all", "no-destructive", "read-only"] {
        assert!(error.contains(spelling), "the error names `{spelling}`: {error}");
    }
}

#[test]
fn test_a_tools_pattern_that_is_empty_or_has_an_empty_part_is_refused() {
    for pattern in ["\"\"", "/", "github/", "/save_comment"] {
        let text = format!("---\nname: developer\ndescription: d\nsmelt:\n  tools:\n    allow: [{pattern}]\n---\n");
        let error = parse_err(DEV, &text);
        assert!(error.contains("empty"), "{pattern}: {error}");
    }
}

#[test]
fn test_a_tools_pattern_with_two_slashes_is_refused() {
    let error = parse_err(DEV, "---\nname: developer\ndescription: d\nsmelt:\n  tools:\n    allow: [github/foo/bar]\n---\n");
    assert!(error.contains("github/foo/bar") && error.contains("more than one"), "{error}");
}

#[test]
fn test_a_star_in_a_tools_pattern_server_is_refused_saying_the_server_is_named() {
    // Quoted: YAML reads a bare scalar starting with `*` as an alias.
    for pattern in ["*/get_*", "git*hub/*"] {
        let text = format!("---\nname: developer\ndescription: d\nsmelt:\n  tools:\n    allow: [\"{pattern}\"]\n---\n");
        let error = parse_err(DEV, &text);
        assert!(error.contains(pattern) && error.contains("server"), "{pattern}: {error}");
    }
}

/// SME-241's report: a rule naming a server that isn't configured rules
/// nothing, and the listing says which patterns those are — the rename
/// edge case, where a server's old name lingers in a persona file.
#[test]
fn test_patterns_naming_no_configured_server_are_the_report() {
    let rules = parse_ok(
        DEV,
        "---\nname: developer\ndescription: d\nsmelt:\n  tools:\n    allow: [github/get_*, gh/create_*, write_file]\n    deny: [linear/*]\n---\n",
    )
    .tools
    .expect("tools");
    let servers = vec!["github".to_string(), "linear".to_string()];
    assert_eq!(
        rules.unmatched_servers(&servers),
        vec!["gh/create_*"],
        "only the pattern naming no configured server, from either list"
    );
    assert_eq!(
        rules.unmatched_servers(&[]),
        vec!["github/get_*", "gh/create_*", "linear/*"],
        "with no server configured every server pattern names nothing; a smelt-tool pattern never does"
    );
}

#[test]
fn test_a_pattern_in_both_allow_and_deny_is_refused_naming_it() {
    let error = parse_err(
        DEV,
        "---\nname: developer\ndescription: d\nsmelt:\n  tools:\n    allow: [github/*, write_file]\n    deny: [github/*]\n---\n",
    );
    assert!(error.contains("github/*") && error.contains("both"), "{error}");
    // Overlap at one level is not a repeat: the design's deny-wins rule
    // decides that call, so these two lists parse.
    parse_ok(
        DEV,
        "---\nname: developer\ndescription: d\nsmelt:\n  tools:\n    allow: [github/get_*]\n    deny: [github/*_pull_*]\n---\n",
    );
}
