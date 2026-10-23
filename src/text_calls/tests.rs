use super::*;

/// The one call `text` holds, by name.
fn one_name(text: &str) -> Option<String> {
    let calls = find(text);
    assert_eq!(calls.len(), 1, "exactly one call in {text:?}: {calls:?}");
    calls.into_iter().next().expect("one call").name
}

/// U1: each form the chat templates produce is found once, and named
/// where the form names its tool. Each fixture is a model output from
/// llama.cpp's `tests/test-chat.cpp` at `31288c9c6` (the deployed build),
/// its C++ escapes decoded; the line each comes from is cited.
#[test]
fn test_every_form_is_found_and_named() {
    let cases: &[(&str, &str, Option<&str>)] = &[
        // Qwen3-Coder / Qwen3.5 XML, test-chat.cpp:2173.
        ("qwen3_coder", include_str!("fixtures/qwen3_coder.txt"), Some("special_function")),
        // Hermes 2/3, Qwen 2.5/3 JSON, test-chat.cpp:5551.
        ("hermes", include_str!("fixtures/hermes.txt"), Some("special_function")),
        // GLM-4.6, test-chat.cpp:4406.
        ("glm", include_str!("fixtures/glm.txt"), Some("special_function")),
        // MiniMax-M2, test-chat.cpp:5684.
        ("minimax", include_str!("fixtures/minimax.txt"), Some("empty_args")),
        // Seed-OSS, test-chat.cpp:3469.
        ("seed_oss", include_str!("fixtures/seed_oss.txt"), Some("special_function")),
        // Mistral `[TOOL_CALLS]NAME[ARGS]`, test-chat.cpp:2629.
        ("mistral", include_str!("fixtures/mistral.txt"), Some("special_function")),
        // DeepSeek V3.1, test-chat.cpp:3936.
        ("deepseek_v3_1", include_str!("fixtures/deepseek_v3_1.txt"), Some("get_time")),
        // Kimi K2, test-chat.cpp:5210: `functions.NAME:0` is the call's id.
        ("kimi_k2", include_str!("fixtures/kimi_k2.txt"), Some("special_function")),
        // Gemma 4, test-chat.cpp:3031.
        ("gemma4", include_str!("fixtures/gemma4.txt"), Some("get_time")),
        // LFM2, test-chat.cpp:2032.
        ("lfm2", include_str!("fixtures/lfm2.txt"), Some("special_function")),
        // gpt-oss, test-chat.cpp:6742.
        ("gpt_oss", include_str!("fixtures/gpt_oss.txt"), Some("special_function")),
        // Llama 3.1 / Functionary v3.1 `<function=NAME>{…}`, test-chat.cpp:6173.
        ("functionary_v3_1", include_str!("fixtures/functionary_v3_1.txt"), Some("special_function")),
        // DeepSeek V3.2 DSML, test-chat.cpp:4037 (the format comment is at :4012).
        ("deepseek_v3_2_dsml", include_str!("fixtures/deepseek_v3_2_dsml.txt"), Some("get_time")),
        // The evidence: conversation 2294639360's message 98 (prose, then
        // the markup) and message 96's second text block, verbatim.
        ("evidence_98", include_str!("fixtures/evidence_98.txt"), Some("mcp__linear.app__add_issue_comment")),
        ("evidence_96", include_str!("fixtures/evidence_96.txt"), Some("mcp__linear.app__issue_read")),
    ];
    for (form, text, name) in cases {
        assert_eq!(one_name(text).as_deref(), *name, "{form}");
    }
}

/// U1: a form whose name can't be read still counts, unnamed; a name
/// that isn't a name (other characters, or too long) is unreadable.
#[test]
fn test_a_call_whose_name_cant_be_read_is_found_unnamed() {
    assert_eq!(one_name("[TOOL_CALLS][{\"name\": \"x\"}]"), None);
    assert_eq!(one_name("<tool_call>\n{\"name\": \"bad name!\", \"arguments\": {}}</tool_call>"), None);
    let long = format!("<tool_call>\n<function={}>\n</function>\n</tool_call>", "a".repeat(129));
    assert_eq!(one_name(&long), None);
    let longest = format!("<tool_call>\n<function={}>\n</function>\n</tool_call>", "a".repeat(128));
    assert_eq!(one_name(&longest), Some("a".repeat(128)));
}

/// Two calls in one reply are two, and the `<function_calls>` block's
/// first `<invoke>` isn't counted twice.
#[test]
fn test_two_calls_are_two() {
    let qwen = include_str!("fixtures/qwen3_coder.txt");
    let calls = find(&format!("{qwen}\n{}", qwen.replace("special_function", "other")));
    let names: Vec<Option<&str>> = calls.iter().map(|c| c.name.as_deref()).collect();
    assert_eq!(names, [Some("special_function"), Some("other")]);
    let claude = "<function_calls>\n<invoke name=\"a\">\n<parameter name=\"x\">1</parameter>\n</invoke>\n\
                  <invoke name=\"b\">\n</invoke>\n</function_calls>";
    let names: Vec<Option<String>> = find(claude).into_iter().map(|c| c.name).collect();
    assert_eq!(names, [Some("a".to_string()), Some("b".to_string())]);
}

/// U2: markup that is quoted, or prose that mentions it, isn't a call.
#[test]
fn test_quoted_markup_and_prose_are_not_calls() {
    let qwen = include_str!("fixtures/qwen3_coder.txt");
    let indented: String = qwen.lines().map(|l| format!("    {l}\n")).collect();
    let quoted: String = qwen.lines().map(|l| format!("> {l}\n")).collect();
    let cases = [
        format!("Here it is:\n\n```xml\n{qwen}\n```\nThat's the form."),
        format!("Here it is:\n\n~~~\n{qwen}\n~~~\n"),
        format!("Unclosed:\n```\n{qwen}"),
        format!("Indented fence:\n   ````\n{qwen}\n   ````\n"),
        "Write `<tool_call>` then `<function=name>` on the next line.".to_string(),
        "A call: `<tool_call>\n<function=x>`".to_string(),
        format!("Quoted:\n{quoted}"),
        format!("Indented:\n\n{indented}"),
        "Quoted:\n> [TOOL_CALLS]get_time[ARGS]{}".to_string(),
        "Indented:\n\n    [TOOL_CALLS]get_time[ARGS]{}".to_string(),
        "Tab-indented:\n\n\t<|tool_call>call:get_time{}".to_string(),
        "I'll wrap it in <tool_call> tags.".to_string(),
        "<tool_call> tags are how Qwen marks calls.".to_string(),
        "<tool_call>\nthen the function line\n</tool_call>".to_string(),
        "{\"name\": \"get_time\", \"arguments\": {\"city\": \"XYZCITY\"}}".to_string(),
        "| marker | model |\n|--|--|\n| `<tool_call>` | Qwen |\n| `[TOOL_CALLS]` | Mistral |\n| `<|tool_call_begin|>` | Kimi |".to_string(),
        "Mistral starts a call with `[TOOL_CALLS]`.".to_string(),
        "A gpt-oss call says `to=functions.x<|channel|>`.".to_string(),
        "<function=name> is how it starts, then the arguments.".to_string(),
        "<invoke name=\"x\"> wraps a call.".to_string(),
        "Text with a function=x> in it and to=functions.y in it.".to_string(),
        // This ticket's own description: its markup sits in a fence.
        include_str!("fixtures/sme_286_description.txt").to_string(),
    ];
    let found: Vec<(&String, Vec<TextCall>)> = cases.iter().map(|text| (text, find(text))).filter(|(_, calls)| !calls.is_empty()).collect();
    assert!(found.is_empty(), "calls found in: {found:#?}");
}

/// U2: special-token markers count mid-line, outside code.
#[test]
fn test_a_special_token_counts_mid_line() {
    assert_eq!(one_name("Calling it now: [TOOL_CALLS]get_time[ARGS]{}").as_deref(), Some("get_time"));
}

/// The tools on offer in the evidence conversations: the natives and
/// GitHub tools they called, and the Linear MCP server's whole list.
const OFFERED: &[&str] = &[
    "clone_repo", "create_pod", "create_terminal", "edit_file", "grep", "list_commands", "list_directory",
    "list_personas", "list_terminals", "load_instructions", "read_file", "read_terminal_output",
    "run_terminal_command", "send_signal", "set_idle_notice", "terminal_command_status", "terminate_pod",
    "terminate_terminal", "todoread", "todowrite", "write_file",
    "mcp__Github__create_pull_request", "mcp__Github__get_me", "mcp__Github__list_pull_requests",
    "mcp__Github__pull_request_read", "mcp__Github__update_pull_request",
    "mcp__linear.app__create_attachment", "mcp__linear.app__create_issue_label",
    "mcp__linear.app__delete_attachment", "mcp__linear.app__delete_comment",
    "mcp__linear.app__delete_status_update", "mcp__linear.app__extract_images",
    "mcp__linear.app__get_attachment", "mcp__linear.app__get_document", "mcp__linear.app__get_issue",
    "mcp__linear.app__get_issue_status", "mcp__linear.app__get_milestone", "mcp__linear.app__get_project",
    "mcp__linear.app__get_status_updates", "mcp__linear.app__get_team", "mcp__linear.app__get_user",
    "mcp__linear.app__list_comments", "mcp__linear.app__list_cycles", "mcp__linear.app__list_documents",
    "mcp__linear.app__list_issue_labels", "mcp__linear.app__list_issue_statuses",
    "mcp__linear.app__list_issues", "mcp__linear.app__list_milestones",
    "mcp__linear.app__list_project_labels", "mcp__linear.app__list_projects",
    "mcp__linear.app__list_teams", "mcp__linear.app__list_users", "mcp__linear.app__save_comment",
    "mcp__linear.app__save_document", "mcp__linear.app__save_issue", "mcp__linear.app__save_milestone",
    "mcp__linear.app__save_project", "mcp__linear.app__save_status_update",
    "mcp__linear.app__search_documentation",
];

/// U3: the evidence's invented names list the tools the model meant
/// first, on the server it named.
#[test]
fn test_similar_names_the_tools_meant_first() {
    let comment = similar("mcp__linear.app__add_issue_comment", OFFERED);
    assert_eq!(comment.first().map(String::as_str), Some("mcp__linear.app__save_comment"), "{comment:?}");
    assert!(comment.contains(&"mcp__linear.app__list_comments".to_string()), "{comment:?}");
    assert!(comment.len() <= 5, "{comment:?}");
    let read = similar("mcp__linear.app__issue_read", OFFERED);
    assert_eq!(read.first().map(String::as_str), Some("mcp__linear.app__get_issue"), "{read:?}");
}

/// U3: a tool on the named server outranks another server's with the
/// same words; one sharing no word isn't similar.
#[test]
fn test_similar_prefers_the_named_server() {
    let offered = ["mcp__b__save_comment", "mcp__a__save_comment", "mcp__a__list_teams"];
    assert_eq!(similar("mcp__a__add_comment", &offered), ["mcp__a__save_comment", "mcp__b__save_comment"]);
    assert_eq!(similar("mcp__a__frobnicate", &offered), Vec::<String>::new());
    assert_eq!(similar("frobnicate", OFFERED), Vec::<String>::new());
}

/// U3/U4: a name on a server that offers nothing says so; one on a
/// server whose name differs only in case is that server.
#[test]
fn test_the_notice_says_when_the_server_isnt_offered() {
    let notice = text_call_notice(&[TextCall { name: Some("mcp__github__issue_read".to_string()) }], &["mcp__linear.app__get_issue", "read_file"]);
    assert!(notice.contains("no MCP server `github` is offered"), "{notice}");
    let notice = text_call_notice(&[TextCall { name: Some("mcp__github__pull_request_read".to_string()) }], OFFERED);
    assert!(!notice.contains("no MCP server"), "{notice}");
    assert!(notice.contains("`mcp__Github__pull_request_read`"), "{notice}");
}

/// U4: the notice names every call and its suggestions, and the
/// transcript reads the names back from it.
#[test]
fn test_the_notice_round_trips() {
    let call = |name: Option<&str>| TextCall { name: name.map(str::to_string) };
    let cases: Vec<Vec<TextCall>> = vec![
        vec![call(Some("mcp__linear.app__add_issue_comment"))],
        vec![call(Some("read_file"))],
        vec![call(Some("frobnicate"))],
        vec![call(None)],
        vec![call(Some("mcp__linear.app__issue_read")), call(None)],
        vec![call(Some("mcp__github__issue_read")), call(Some("todowrite"))],
    ];
    for calls in cases {
        let notice = text_call_notice(&calls, OFFERED);
        assert!(notice.starts_with(TEXT_CALL_NOTICE_START), "{notice}");
        let names: Vec<Option<String>> = calls.iter().map(|c| c.name.clone()).collect();
        assert_eq!(parse_text_call_notice(&notice), Some(names), "{notice}");
    }
    let notice = text_call_notice(&[call(Some("mcp__linear.app__add_issue_comment"))], OFFERED);
    assert!(notice.contains("`mcp__linear.app__save_comment`"), "{notice}");
    assert!(text_call_notice(&[call(Some("read_file"))], OFFERED).contains("`read_file` is offered"));
    assert!(text_call_notice(&[call(Some("frobnicate"))], OFFERED).contains("no offered tool has a similar name"));
    assert!(text_call_notice(&[call(None)], OFFERED).contains("couldn't be read"));
}

/// U4: other notices and plain text aren't text-call notices.
#[test]
fn test_other_text_is_not_a_text_call_notice() {
    for text in [
        "",
        "hello",
        EMPTY_REPLY_NOTICE,
        TEXT_CALL_NOTICE_START,
        crate::api::chat::STOP_NOTICE,
        &format!("{TEXT_CALL_NOTICE_START} `a b` is offered."),
    ] {
        assert_eq!(parse_text_call_notice(text), None, "{text:?}");
    }
}

/// U4: the notice quotes only tool names, never a call's parameters.
#[test]
fn test_the_notice_never_quotes_parameters() {
    let text = "<tool_call>\n<function=mcp__x__post>\n<parameter=body>\nSECRET-PARAM-286\n</parameter>\n</function>\n</tool_call>";
    let calls = find(text);
    assert_eq!(calls.len(), 1);
    let notice = text_call_notice(&calls, OFFERED);
    assert!(!notice.contains("SECRET-PARAM-286"), "{notice}");
    assert!(!bound_error(&calls).contains("SECRET-PARAM-286"));
}

/// Review 1, finding 2: special-token markers named in prose, inline code
/// broken over lines, markup straight after a code span, and reasoning
/// leaked into the text inside `<think>` aren't calls.
#[test]
fn test_markers_without_their_structure_are_not_calls() {
    let cases = [
        "Mistral marks calls with [TOOL_CALLS] before the JSON list.",
        "Kimi's format starts <|tool_calls_section_begin|> then the calls.",
        "Kimi: <|tool_call_begin|>functions.x:0 begins one.",
        "DeepSeek's starts <\u{ff5c}tool\u{2581}calls\u{2581}begin\u{ff5c}> and <\u{ff5c}tool\u{2581}call\u{2581}begin\u{ff5c}> then a name.",
        "Gemma writes <|tool_call>call: then the name.",
        "LFM2 uses <|tool_call_start|> markers.",
        "Use `\n<function=foo>\n<parameter=a>` here.",
        "`x`<tool_call>\n<function=foo>\n</function>\n</tool_call>",
        "<think>I'll call it:\n<tool_call>\n<function=foo>\n</function>\n</tool_call>\n</think>\nDone.",
    ];
    let found: Vec<(&str, Vec<TextCall>)> = cases.iter().map(|text| (*text, find(text))).filter(|(_, calls)| !calls.is_empty()).collect();
    assert!(found.is_empty(), "calls found in: {found:#?}");
}

