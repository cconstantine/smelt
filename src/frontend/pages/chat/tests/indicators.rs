use super::*;

fn terminal_with(commands: &[(&str, &str, Option<i32>)]) -> SandboxTerminalPanelEntry {
    let mut terminal = test_sandbox_terminal_entry(10, 1);
    for (i, (command, status, code)) in commands.iter().enumerate() {
        let mut entry = test_sandbox_command_entry(&format!("cmd-{i}"), command);
        entry.status = status.to_string();
        entry.exit_code = *code;
        terminal.commands.push(entry);
    }
    terminal
}

/// SME-144: the titlebar's pill reads the terminal's last command.
#[test]
fn test_latest_command_indicator_reads_the_last_command() {
    let cases: [(&[(&str, &str, Option<i32>)], CommandIndicator); 7] = [
        (&[], CommandIndicator::None),
        (&[("sleep 9", "running", None)], CommandIndicator::Running),
        (&[("true", "finished", Some(0))], CommandIndicator::Exited(0)),
        (&[("false", "finished", Some(3))], CommandIndicator::Exited(3)),
        (&[("make", "lost", None)], CommandIndicator::Lost),
        (&[("make", "finished", None)], CommandIndicator::Other("finished".into())),
        (&[("false", "finished", Some(1)), ("sleep 9", "running", None)], CommandIndicator::Running),
    ];
    for (commands, expected) in cases {
        assert_eq!(latest_command_indicator(&terminal_with(commands)), expected, "{commands:?}");
    }
}

#[test]
fn test_each_command_indicator_has_a_label_class_and_title() {
    let cases = [
        (CommandIndicator::Running, "running", "task-terminal-command task-terminal-command-running", "running"),
        (CommandIndicator::Exited(0), "exit 0", "task-terminal-command task-terminal-command-ok", "exited with status 0"),
        (CommandIndicator::Exited(130), "exit 130", "task-terminal-command task-terminal-command-failed", "exited with status 130"),
        (CommandIndicator::Lost, "lost", "task-terminal-command task-terminal-command-lost", "lost"),
        (CommandIndicator::Other("weird".into()), "weird", "task-terminal-command task-terminal-command-other", "weird"),
    ];
    for (indicator, label, class, says) in cases {
        assert_eq!(indicator.label(), label);
        assert_eq!(indicator.class(), class);
        assert!(!indicator.glyph().is_empty(), "{indicator:?} has no glyph");
        let title = indicator.title("cargo build --release");
        assert!(title.starts_with("Last command: \u{2068}cargo build --release\u{2069}"), "{title}");
        assert!(title.contains(says), "{title}");
    }
    assert!(CommandIndicator::Lost.title("x").contains("no exit status"));
}

/// SME-144 review 1: a long or multi-line command (a heredoc the model
/// wrote) isn't the whole tooltip and accessible name: its first line with
/// anything in it, cut at 120 characters, isolated so bidi controls in it
/// can't reorder the status after it (review 2).
#[test]
fn test_a_command_indicators_title_shortens_a_long_command() {
    let shown = |command: &str| format!("\u{2068}{command}\u{2069}");
    let long = format!("cat > big.txt <<'EOF'\n{}\nEOF", "x".repeat(5000));
    assert_eq!(
        CommandIndicator::Running.title(&long),
        format!("Last command: {} \u{b7} running", shown("cat > big.txt <<'EOF'\u{2026}"))
    );
    let wide = "y".repeat(300);
    assert_eq!(
        CommandIndicator::Exited(1).title(&wide),
        format!("Last command: {} \u{b7} exited with status 1", shown(&format!("{}\u{2026}", "y".repeat(120))))
    );
    assert_eq!(CommandIndicator::Exited(0).title("true"), format!("Last command: {} \u{b7} exited with status 0", shown("true")));
    assert_eq!(CommandIndicator::Exited(0).title("true\n"), format!("Last command: {} \u{b7} exited with status 0", shown("true")));
    // Review 2: leading blank lines are skipped, and say something was.
    assert_eq!(CommandIndicator::Running.title("\n\necho hi"), format!("Last command: {} \u{b7} running", shown("echo hi")));
    assert_eq!(CommandIndicator::Running.title("\n  \necho hi\nmore"), format!("Last command: {} \u{b7} running", shown("echo hi\u{2026}")));
    // An empty or blank command says so, with no stray whitespace.
    assert_eq!(CommandIndicator::Running.title(""), "Last command: (empty) \u{b7} running");
    assert_eq!(CommandIndicator::Running.title("\t\n  \n"), "Last command: (empty) \u{b7} running");
    // A right-to-left override inside stays inside its isolate.
    assert_eq!(CommandIndicator::Running.title("echo \u{202e}abc"), format!("Last command: {} \u{b7} running", shown("echo \u{202e}abc")));
}

/// SME-144 review 2: a live `lost` pill only ever comes from a failed send
/// (a pod restart's crash cleanup removes the terminal's card), so its
/// tooltip doesn't offer a pod restart as the reason.
#[test]
fn test_the_lost_title_names_only_what_can_cause_it() {
    let title = CommandIndicator::Lost.title("make");
    assert!(title.contains("couldn't be sent") && title.contains("no exit status"), "{title}");
    assert!(!title.contains("restart"), "{title}");
}
