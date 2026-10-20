use super::*;

#[test]
fn test_short_age_uses_one_unit() {
    assert_eq!(short_age(-5), "now");
    assert_eq!(short_age(30), "now");
    assert_eq!(short_age(5 * 60 + 20), "5m");
    assert_eq!(short_age(3 * 3600 + 59 * 60), "3h");
    assert_eq!(short_age(2 * 86400 + 5), "2d");
    assert_eq!(short_age(15 * 86400), "2w");
}

#[test]
fn test_format_elapsed_counts_seconds_then_minutes() {
    assert_eq!(format_elapsed(0), "0s");
    assert_eq!(format_elapsed(42), "42s");
    assert_eq!(format_elapsed(125), "2m 05s");
    assert_eq!(format_elapsed(3600), "60m 00s");
}

#[test]
fn test_a_stop_reads_as_stopped() {
    assert_eq!(
        system_notice(crate::api::chat::STOP_NOTICE, &HashMap::new()).as_deref(),
        Some("Stopped.")
    );
}

/// SME-111: a cut-off reply's notice reads as a sentence naming its limit,
/// what bound it and what to change, for each limit.
#[test]
fn test_a_cut_off_notice_reads_as_a_sentence() {
    use crate::api::chat::{ReplyLimit, cut_off_notice};
    assert_eq!(
        system_notice(&cut_off_notice(131_072, ReplyLimit::HalfWindow, None), &HashMap::new()).as_deref(),
        Some("The reply was cut off at its limit of 131,072 tokens (half the context window). A reply can be at most half the context window: lower the model's effort or reasoning budget on its provider's page, or ask for less at once.")
    );
    for limit in ReplyLimit::ALL {
        let shown = system_notice(&cut_off_notice(16_384, limit, None), &HashMap::new()).expect("a notice");
        assert!(shown.contains("16,384") && shown.contains(limit.reason()) && shown.contains(limit.hint()), "{shown}");
    }
    assert_eq!(system_notice("Your last reply was cut off: it reached its limit of lots", &HashMap::new()), None);
}

/// B3 (SME-122): a refusal notice reads as one in the chat, naming a
/// dropped call, and round-trips.
#[test]
fn test_a_refusal_notice_says_the_provider_stopped_the_reply() {
    use crate::api::chat::{parse_refusal_notice, refusal_notice};
    assert_eq!(parse_refusal_notice(&refusal_notice(None)), Some(None));
    assert_eq!(parse_refusal_notice(&refusal_notice(Some("write_file"))), Some(Some("write_file".to_string())));
    assert_eq!(parse_refusal_notice("Something else."), None);
    assert_eq!(
        system_notice(&refusal_notice(None), &HashMap::new()).as_deref(),
        Some("The model's provider stopped this reply (a refusal).")
    );
    assert_eq!(
        system_notice(&refusal_notice(Some("write_file")), &HashMap::new()).as_deref(),
        Some("The model's provider stopped this reply (a refusal) in the middle of a `write_file` call, which didn't run.")
    );
}

/// SME-126: a notice for a reply cut off in the middle of a tool call
/// names the call, for the model and in the chat, and round-trips; a
/// notice saved before SME-126 still parses and reads as it did.
#[test]
fn test_a_cut_off_notice_names_the_call_it_was_cut_in() {
    use crate::api::chat::{ReplyLimit, cut_off_notice, parse_cut_off_notice};
    let notice = cut_off_notice(16_384, ReplyLimit::OutputCap, Some("write_file"));
    assert_eq!(
        notice,
        "Your last reply was cut off: it reached its limit of 16384 tokens (the model's maximum reply length) before it finished. It stopped in the middle of a call to `write_file`, which was dropped and not run."
    );
    assert_eq!(parse_cut_off_notice(&notice), Some((16_384, ReplyLimit::OutputCap, Some("write_file".to_string()))));
    assert_eq!(
        system_notice(&notice, &HashMap::new()).as_deref(),
        Some("The reply was cut off at its limit of 16,384 tokens (the model's maximum reply length) in the middle of a `write_file` call, which didn't run. Raise \u{201c}Max reply tokens\u{201d} for this model on its provider's page, or lower its effort or reasoning budget.")
    );

    let old = "Your last reply was cut off: it reached its limit of 8192 tokens (half the context window) before it finished.";
    assert_eq!(cut_off_notice(8_192, ReplyLimit::HalfWindow, None), old);
    assert_eq!(parse_cut_off_notice(old), Some((8_192, ReplyLimit::HalfWindow, None)));
    assert_eq!(parse_cut_off_notice(&format!("{old} It stopped in the middle of a call to ``, which was dropped and not run.")), None);
    assert_eq!(parse_cut_off_notice(&format!("{old} And more.")), None);
}

#[test]
fn test_system_notices_read_as_short_sentences() {
    let commands = HashMap::from([("abc123".to_string(), "python3 primes.py".to_string())]);
    let notice = |t: &str| system_notice(t, &commands);
    assert_eq!(
        notice("Terminal command abc123 finished: exit code 0.").as_deref(),
        Some("`python3 primes.py` finished (exit code 0)")
    );
    assert_eq!(
        notice("Terminal command zzz finished: exit code 127.").as_deref(),
        Some("A command finished (exit code 127)")
    );
    assert_eq!(
        notice("Terminal command abc123's outcome is unknown — the terminal became unreachable while it was running.").as_deref(),
        Some("`python3 primes.py` stopped: its terminal became unreachable")
    );
    // SME-79: the idle notice reads as one sentence naming the command,
    // without the tool advice written for the model; an unknown id falls
    // back like the others, and text that only starts like a notice is not
    // one.
    assert_eq!(
        notice(r#"Terminal command abc123 ("python3 primes.py", running for 2 hours 5 minutes) is still running and hasn't been touched in 5 minutes: 42 lines on stdout, 0 on stderr. Nothing stopped it and smelt won't stop it. Read what it has printed with read_terminal_output."#).as_deref(),
        Some("`python3 primes.py` is still running, untouched for 5 minutes")
    );
    assert_eq!(
        notice(r#"Terminal command zzz ("sleep 300", running for 1 hour) is still running and hasn't been touched in 1 hour: 0 lines on stdout, 0 on stderr. Nothing stopped it and smelt won't stop it."#).as_deref(),
        Some("A command is still running, untouched for 1 hour")
    );
    assert_eq!(
        notice("Terminal command abc123 is still running, and nobody knows how it goes."),
        None
    );
    assert_eq!(
        notice("Sandbox pod 156 stopped unexpectedly (OOMKilled); every terminal running in it is no longer available.").as_deref(),
        Some("The sandbox stopped unexpectedly (OOMKilled); its terminals are gone")
    );
    assert_eq!(
        notice("Sandbox pod 19 stopped unexpectedly; every terminal running in it is no longer available.").as_deref(),
        Some("The sandbox stopped unexpectedly; its terminals are gone")
    );
    assert_eq!(
        notice("The user stopped sandbox pod 157. Its terminals, and any files outside /workspace and mounted volumes, are gone. Create a new pod if you need one.").as_deref(),
        Some("You stopped the sandbox; its terminals are gone, and /workspace is kept")
    );
    assert_eq!(
        notice(r#"<task-notification task_id="t1" tool="count">finished: Counted to 3</task-notification>"#).as_deref(),
        Some("Background task count (t1) finished: Counted to 3")
    );
    assert_eq!(notice("Please run the tests"), None);
}

/// SME-79 review 2: the quoted command line sits *before* the notice's
/// marker, so a command whose own text contains that phrase moves the split
/// into the middle of the quote — `grep`ping over smelt's own notices is the
/// plain way to do it. The real marker is the *last* one: everything after it
/// is smelt's fixed text.
#[test]
fn test_an_idle_notice_splits_at_its_own_marker() {
    // The phrase smelt writes between the quoted command and its numbers,
    // leading and trailing spaces and all — the marker the sentence is cut at.
    let marker = " is still running and hasn't been touched in ";
    let line = format!(r#"grep -c "{marker}" agent.log"#);
    let notice = format!(
        r#"Terminal command abc123 ("{line}", running for 1 minute){marker}5 minutes: 0 lines on stdout, 0 on stderr. Nothing stopped it and smelt won't stop it."#
    );
    let commands = HashMap::from([("abc123".to_string(), line.clone())]);
    assert_eq!(
        system_notice(&notice, &commands).as_deref(),
        Some(format!("`{line}` is still running, untouched for 5 minutes")).as_deref(),
        "cut at the notice's own marker, not at the one inside the quoted command"
    );
    // An unknown command id still falls back like every other notice's.
    assert_eq!(
        system_notice(&notice, &HashMap::new()).as_deref(),
        Some("A command is still running, untouched for 5 minutes")
    );
}

#[test]
fn test_terminal_commands_are_found_by_id() {
    let messages = vec![
        message_with_blocks(1, "assistant", vec![ContentBlock::ToolUse {
            id: "toolu_1".to_string(),
            name: "run_terminal_command".to_string(),
            input: serde_json::json!({"command": "ls -la", "terminal_id": 1}),
        }]),
        message_with_blocks(2, "user", vec![ContentBlock::ToolResult {
            tool_use_id: "toolu_1".to_string(),
            content: "command sent (id: XYZ)".to_string(),
            is_error: None,
        }]),
    ];
    assert_eq!(terminal_commands_by_id(&messages).get("XYZ").map(String::as_str), Some("ls -la"));
}

#[test]
fn test_tool_summary_says_what_a_call_did() {
    assert_eq!(tool_summary("create_pod", &serde_json::json!({})), "Started the sandbox");
    assert_eq!(tool_summary("terminate_pod", &serde_json::json!({})), "Stopped the sandbox");
    assert_eq!(tool_summary("create_terminal", &serde_json::json!({})), "Opened a terminal");
    assert_eq!(
        tool_summary("run_terminal_command", &serde_json::json!({"command": "python3 primes.py", "terminal_id": 3})),
        "Ran `python3 primes.py`"
    );
    assert_eq!(tool_summary("read_terminal_output", &serde_json::json!({"command_id": "x"})), "Read a command's output");
    assert_eq!(tool_summary("set_idle_notice", &serde_json::json!({"command_id": "x", "idle_notice_seconds": 60})), "Re-timed a command's idle notice");
    assert_eq!(tool_summary("write_file", &serde_json::json!({"path": "/home/sandbox/a.py", "content": "x"})), "Wrote /home/sandbox/a.py");
    assert_eq!(tool_summary("read_file", &serde_json::json!({"path": "/etc/hosts"})), "Read /etc/hosts");
    assert_eq!(tool_summary("list_directory", &serde_json::json!({"path": "/tmp"})), "Listed /tmp");
    assert_eq!(tool_summary("glob", &serde_json::json!({"pattern": "**/*.rs", "path": "/src"})), "Found files matching `**/*.rs`");
    assert_eq!(tool_summary("grep", &serde_json::json!({"pattern": "fn main"})), "Searched files for `fn main`");
    assert_eq!(tool_summary("webfetch", &serde_json::json!({"url": "https://example.com"})), "Read https://example.com");
    assert_eq!(tool_summary("http_request", &serde_json::json!({"url": "https://api.x/y", "method": "POST"})), "Sent POST https://api.x/y");
    assert_eq!(tool_summary("http_request", &serde_json::json!({"url": "https://api.x/y"})), "Sent GET https://api.x/y");
    assert_eq!(tool_summary("browser_navigate", &serde_json::json!({"url": "https://example.com"})), "Opened https://example.com in the browser");
    assert_eq!(tool_summary("todowrite", &serde_json::json!({"todos": []})), "Updated the todo list");
    assert_eq!(
        tool_summary("clone_repo", &serde_json::json!({"url": "git@github.com:o/r.git"})),
        "Cloned git@github.com:o/r.git"
    );
}

#[test]
fn test_instructions_source_names_repo_commit_and_size() {
    let mut doc = crate::git::ProjectInstructions {
        repo_url: "git@github.com:o/r.git".to_string(),
        path: "/workspace/r".to_string(),
        commit: Some("43835b44f939".to_string()),
        content: "Run make test.\n".to_string(),
        file_bytes: 15,
        truncated: false,
    };
    assert_eq!(instructions_source(&doc), "git@github.com:o/r.git at 43835b4 \u{b7} 15 bytes");
    doc.file_bytes = 50_000;
    doc.truncated = true;
    assert_eq!(
        instructions_source(&doc),
        "git@github.com:o/r.git at 43835b4 \u{b7} 50000 bytes, only the first 32 KiB loaded"
    );
}

#[test]
fn test_instructions_label_says_which_agents_md_files_are_loaded() {
    let mut repo = RepoSummary {
        id: 1,
        url: "u".to_string(),
        path: "/workspace/r".to_string(),
        requested_branch: None,
        branch: None,
        commit: None,
        status: RepoStatus::Ready,
        error: None,
        agents_files: vec![],
        loaded_instructions: vec![],
        trust_requests: vec![],
    };
    assert_eq!(instructions_label(&repo), None, "no AGENTS.md files");
    repo.agents_files = vec!["AGENTS.md".to_string(), "web/AGENTS.md".to_string()];
    assert_eq!(instructions_label(&repo).as_deref(), Some("AGENTS.md not loaded"));
    repo.loaded_instructions = vec!["AGENTS.md".to_string(), "web/AGENTS.md".to_string()];
    assert_eq!(instructions_label(&repo).as_deref(), Some("Loaded AGENTS.md, web/AGENTS.md"));
}

#[test]
fn test_repo_detail_says_what_is_checked_out() {
    let mut repo = RepoSummary {
        id: 1,
        url: "git@github.com:o/r.git".to_string(),
        path: "/workspace/r".to_string(),
        requested_branch: Some("dev".to_string()),
        branch: None,
        commit: None,
        status: RepoStatus::Cloning,
        error: None,
        agents_files: vec![],
        loaded_instructions: vec![],
        trust_requests: vec![],
    };
    assert_eq!(repo_detail(&repo), "Cloning dev\u{2026}");
    repo.requested_branch = None;
    assert_eq!(repo_detail(&repo), "Cloning\u{2026}");
    repo.status = RepoStatus::Ready;
    repo.branch = Some("main".to_string());
    repo.commit = Some("43835b44f939c268b73b49292428911526a51508".to_string());
    assert_eq!(repo_detail(&repo), "main \u{b7} 43835b4");
    repo.status = RepoStatus::Failed;
    assert_eq!(repo_detail(&repo), "Clone failed");
    assert_eq!(tool_summary("sandbox_preview_url", &serde_json::json!({"port": 5173})), "Shared a preview of port 5173");
    assert_eq!(
        tool_summary("sandbox_preview_url", &serde_json::json!({"port": 3000, "host": "172.21.0.2"})),
        "Shared a preview of 172.21.0.2:3000"
    );
    assert_eq!(
        tool_summary("mcp__exa__web_search_exa", &serde_json::json!({"query": "rust 1.0 release"})),
        "Searched the web for \"rust 1.0 release\""
    );
    assert_eq!(tool_summary("mcp__github__create_issue", &serde_json::json!({})), "Used create_issue (github)");
    assert_eq!(tool_summary("something_new", &serde_json::json!({})), "Used something_new");
    // A tool that no longer exists still reads sensibly in old history.
    assert_eq!(tool_summary("run_async", &serde_json::json!({"tool": "count"})), "Used run_async");
    assert_eq!(tool_summary("lsp_servers", &serde_json::json!({})), "Listed the language servers");
    assert_eq!(tool_summary("start_language_server", &serde_json::json!({"name": "pyright"})), "Started pyright");
    let lsp = |input| tool_summary("lsp", &input);
    assert_eq!(
        lsp(serde_json::json!({"operation": "incoming_calls", "path": "/workspace/a.rs", "line": 9, "character": 4})),
        "Looked up incoming calls at /workspace/a.rs:9"
    );
    assert_eq!(lsp(serde_json::json!({"operation": "workspace_symbols", "query": "Square"})), "Searched symbols for `Square`");
    assert_eq!(lsp(serde_json::json!({"operation": "document_symbols", "path": "/workspace/a.rs"})), "Listed the symbols in /workspace/a.rs");
    assert_eq!(lsp(serde_json::json!({"operation": "diagnostics", "path": "/workspace/a.rs"})), "Checked /workspace/a.rs for problems");
    assert_eq!(
        lsp(serde_json::json!({"operation": "rename", "path": "/workspace/a.rs", "line": 4, "new_name": "sum_areas"})),
        "Renamed a symbol to sum_areas"
    );
}
