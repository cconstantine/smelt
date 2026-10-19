use super::*;

/// SME-13: a terminal command's own exit wakes the model, with no other
/// trigger. The test's database has no model provider, so the wake fails
/// before any model call, but the notice is saved first and the failure
/// is published: both seen without any model, real or mock.
#[sqlx::test]
async fn test_a_finished_command_wakes_the_model(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        let terminal = create_terminal(&pool, conversation.id).await.expect("create_terminal");
        let mut wake_events = events::subscribe(conversation.id);
        let command_id = "wake";
        db::create_terminal_command(&pool, conversation.id, terminal, command_id, "true", 300)
            .await
            .expect("create_terminal_command");
        send_command(&pool, terminal, command_id, "true").await.expect("send_command");

        let saw_wake_failure = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match wake_events.recv().await {
                    Ok(events::ConversationEvent::NotificationDeliveryFailed { .. }) => return true,
                    Ok(_) => continue,
                    Err(_) => return false,
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(
            saw_wake_failure,
            "the command's own exit should have woken the model (seen as NotificationDeliveryFailed, since \
             the test's database has no model provider), with no other trigger"
        );
        let messages = db::list_messages(&pool, conversation.id).await.expect("list_messages");
        assert!(
            messages.iter().any(|m| {
                m.blocks().ok().is_some_and(|blocks| {
                    blocks.iter().any(|b| {
                        matches!(
                            b,
                            crate::anthropic::ContentBlock::Text { text }
                                if text.contains(command_id) && text.contains("finished")
                        )
                    })
                })
            }),
            "the notice itself should be saved even though the follow-up call failed, got: {messages:?}"
        );
    })
    .await;
}

/// SME-79: the promise, against a real pod. A command nobody touches is
/// noticed, and the notice stops nothing: the row is still `running` when the
/// notice is saved, and the command then ends by itself and gets its finish
/// notice like any other — an idle notice doesn't use that one up.
#[sqlx::test]
async fn test_an_untouched_command_is_noticed_and_keeps_running(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        let terminal = create_terminal(&pool, conversation.id).await.expect("create_terminal");
        let command_id = "idle-sleep";
        // A 3-second interval on a 12-second sleep, so the notice and the
        // command's own end are clearly separate events, not one race.
        db::create_terminal_command(&pool, conversation.id, terminal, command_id, "sleep 12", 3)
            .await
            .expect("create_terminal_command");
        send_command(&pool, terminal, command_id, "sleep 12").await.expect("send_command");

        tokio::time::sleep(Duration::from_secs(4)).await;
        let woken = notice_idle_commands_once(&pool).await;
        assert!(
            woken.contains(&conversation.id),
            "the pass should wake the conversation holding the due command, got {woken:?}"
        );

        // Its drain saves the notice; the delivery that follows fails for want
        // of a model provider, as in SME-13's test — the notice is saved first
        // either way. Poll for it rather than guess at a delay.
        let noticed = tokio::time::timeout(Duration::from_secs(25), async {
            loop {
                let messages = db::list_messages(&pool, conversation.id).await.expect("list_messages");
                if messages.iter().any(|m| {
                    m.blocks().ok().is_some_and(|blocks| {
                        blocks.iter().any(|b| {
                            matches!(
                                b,
                                crate::anthropic::ContentBlock::Text { text }
                                    if text.contains(command_id) && text.contains("still running")
                                    && text.contains("Nothing stopped it")
                            )
                        })
                    })
                }) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .unwrap_or(false);
        assert!(noticed, "the idle notice should be saved naming the command and that nothing stopped it");

        let command = db::get_terminal_command(&pool, command_id)
            .await
            .expect("query")
            .expect("the command row");
        assert_eq!(
            command.status, "running",
            "smelt noticed the command and stopped nothing — SME-79's whole point"
        );
        assert!(command.idle_notice_at.is_some(), "the notice is marked, restarting the clock");
        assert!(command.notified_at.is_none(), "an idle notice is not the finish notice");

        // And it ends on its own, at about the 12 seconds it asked for, with
        // its finish notice taken.
        let finished = tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                let row = db::get_terminal_command(&pool, command_id)
                    .await
                    .expect("query")
                    .expect("the command row");
                if row.status == "finished" && row.notified_at.is_some() {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .unwrap_or(false);
        assert!(finished, "the command should finish by itself and get its finish notice");
    })
    .await;
}

/// A file written in one conversation's pod is invisible from another's.
#[sqlx::test]
async fn test_two_conversations_pods_are_isolated(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let mut terminals = Vec::new();
        for _ in 0..2 {
            let conversation = db::create_conversation(&pool).await.expect("create conversation");
            create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
            terminals.push((conversation.id, create_terminal(&pool, conversation.id).await.expect("create_terminal")));
        }
        let [(a, terminal_a), (b, terminal_b)] = terminals[..] else { unreachable!("two terminals") };
        run_and_wait(&pool, a, terminal_a, "write-marker", "echo marker > /tmp/isolation-marker").await;
        let check = run_and_wait(&pool, b, terminal_b, "check-marker", "cat /tmp/isolation-marker 2>&1; echo EXIT:$?").await;
        let lines = db::read_terminal_output(&pool, &check, &["stdout"], 0, 10).await.expect("read_terminal_output");
        let joined = lines.iter().map(|l| l.data.as_str()).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains("EXIT:1") || joined.contains("No such file"),
            "the second conversation's pod should not see a file written in the first's, got: {joined}"
        );
    })
    .await;
}

/// `write_file`, `read_file`, `edit_file` and `list_directory`,
/// pod-scoped. Hashes guard every change: a file changed underneath (here
/// by a terminal command) refuses a stale hash rather than being
/// clobbered. `expected_line` picks one of several matches, `replace_all`
/// takes them all, an ambiguous match, a missing file and an oversized
/// write are clear errors, and a write makes its parent directories.
#[sqlx::test]
async fn test_file_tools_edit_safely_against_stale_hashes(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let id = conversation.id;
        create_pod(&pool, id, PodLimitOverrides::default()).await.expect("create_pod");
        let terminal = create_terminal(&pool, id).await.expect("create_terminal");
        let file_path = "/tmp/file-tools-test/example.txt";

        // write_file creates a new file: no expected_hash needed.
        let hash1 = write_file(&pool, id, file_path, "line one\nline two\n", None)
            .await
            .expect("write_file (create) should succeed");
        // read_file returns numbered lines, total_lines, and the same hash
        // write_file just returned.
        let read1 = read_file(&pool, id, file_path, 1, 10).await.expect("read_file should succeed");
        assert_eq!(read1.lines, vec!["line one", "line two"]);
        assert_eq!(read1.total_lines, 2);
        assert_eq!(read1.hash, hash1);

        // edit_file with the right expected_hash succeeds and returns a new hash.
        let hash2 = edit_file(&pool, id, file_path, "line one", "line ONE", false, hash1.clone(), None)
            .await
            .expect("edit_file should succeed");
        assert_ne!(hash2, hash1);
        let read2 = read_file(&pool, id, file_path, 1, 10).await.expect("read_file should succeed");
        assert_eq!(read2.lines, vec!["line ONE", "line two"]);
        assert_eq!(read2.hash, hash2);

        // A terminal command changes the file under a stale hash:
        // edit_file and write_file refuse rather than clobber it.
        run_and_wait(&pool, id, terminal, "external-write", &format!("echo changed_externally > {file_path}")).await;
        let stale_edit = edit_file(&pool, id, file_path, "line ONE", "line X", false, hash2.clone(), None).await;
        assert!(
            matches!(stale_edit, Err(TerminalError::Agent(AgentRequestError::Rejected(_)))),
            "edit_file should refuse a stale expected_hash, got {stale_edit:?}"
        );
        let stale_write = write_file(&pool, id, file_path, "clobber", Some(hash2.clone())).await;
        assert!(
            matches!(stale_write, Err(TerminalError::Agent(AgentRequestError::Rejected(_)))),
            "write_file should refuse a stale expected_hash, got {stale_write:?}"
        );
        let read3 = read_file(&pool, id, file_path, 1, 10).await.expect("read_file should succeed");
        assert_eq!(read3.lines, vec!["changed_externally"]);

        // write_file with the current hash succeeds (a legitimate overwrite).
        let hash4 = write_file(&pool, id, file_path, "alpha\nalpha\nbeta\n", Some(read3.hash.clone()))
            .await
            .expect("write_file (overwrite) with the current hash should succeed");

        // expected_line picks one of several identical lines.
        let hash5 = edit_file(&pool, id, file_path, "alpha", "ALPHA", false, hash4.clone(), Some(2))
            .await
            .expect("edit_file with expected_line should succeed");
        let read5 = read_file(&pool, id, file_path, 1, 10).await.expect("read_file should succeed");
        assert_eq!(read5.lines, vec!["alpha", "ALPHA", "beta"], "expected_line should have targeted only the second occurrence");
        assert_eq!(read5.hash, hash5);

        // replace_all replaces every occurrence.
        let hash6 = write_file(&pool, id, file_path, "dup\ndup\ndup\n", Some(read5.hash.clone()))
            .await
            .expect("write_file (overwrite) should succeed");
        edit_file(&pool, id, file_path, "dup", "rep", true, hash6.clone(), None)
            .await
            .expect("edit_file with replace_all should succeed");
        let read7 = read_file(&pool, id, file_path, 1, 10).await.expect("read_file should succeed");
        assert_eq!(read7.lines, vec!["rep", "rep", "rep"]);

        // An ambiguous match without replace_all or expected_line is a clear error.
        let ambiguous = edit_file(&pool, id, file_path, "rep", "x", false, read7.hash.clone(), None).await;
        assert!(
            matches!(ambiguous, Err(TerminalError::Agent(AgentRequestError::Rejected(_)))),
            "expected an ambiguous-match error, got {ambiguous:?}"
        );

        // read_file on a missing path is a clear error, not a panic.
        let missing = read_file(&pool, id, "/tmp/file-tools-test/does-not-exist.txt", 1, 10).await;
        assert!(
            matches!(missing, Err(TerminalError::Agent(AgentRequestError::Rejected(_)))),
            "expected a file error, got {missing:?}"
        );

        // Oversized content is refused, not silently truncated.
        let oversized_content = "x".repeat(300 * 1024); // over the 256 KiB cap
        let oversized = write_file(&pool, id, "/tmp/file-tools-test/big.txt", &oversized_content, None).await;
        assert!(
            matches!(oversized, Err(TerminalError::Agent(AgentRequestError::Rejected(_)))),
            "expected a size-limit error, got {oversized:?}"
        );

        // list_directory: not recursive, sorted, the right type and size;
        // the nested path shows write_file makes parent directories.
        write_file(&pool, id, "/tmp/file-tools-test/subdir/nested.txt", "nested", None)
            .await
            .expect("write_file should create parent directories");
        let listing = list_directory(&pool, id, "/tmp/file-tools-test").await.expect("list_directory should succeed");
        let names: Vec<&str> = listing.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["example.txt", "subdir"], "entries should be sorted alphabetically, size-limit failure excluded");
        let example_entry = listing.iter().find(|e| e.name == "example.txt").expect("example.txt should be listed");
        assert!(!example_entry.is_dir);
        assert!(example_entry.size.unwrap_or(0) > 0);
        let subdir_entry = listing.iter().find(|e| e.name == "subdir").expect("subdir should be listed");
        assert!(subdir_entry.is_dir);
    })
    .await;
}

/// SME-19: `glob` matches at every depth with `**` and only the top level
/// with `*`; `grep` searches every file under its path, and its glob
/// filter narrows that.
#[sqlx::test]
async fn test_glob_and_grep_find_files_and_matches(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let id = conversation.id;
        create_pod(&pool, id, PodLimitOverrides::default()).await.expect("create_pod");
        let dir = "/tmp/file-tools-test/glob-grep";
        write_file(&pool, id, &format!("{dir}/one.rs"), "fn one() {}\n", None).await.expect("write_file should succeed");
        write_file(&pool, id, &format!("{dir}/nested/two.rs"), "fn two() {}\n", None)
            .await
            .expect("write_file should create parent directories");
        write_file(&pool, id, &format!("{dir}/notes.txt"), "just plain prose, nothing to match\n", None)
            .await
            .expect("write_file should succeed");

        let recursive_glob = glob(&pool, id, dir, "**/*.rs", 1, 50).await.expect("glob should succeed");
        let mut recursive_paths = recursive_glob.paths.clone();
        recursive_paths.sort();
        assert_eq!(
            recursive_paths,
            vec![format!("{dir}/nested/two.rs"), format!("{dir}/one.rs")],
            "**/*.rs should match .rs files at every depth"
        );
        assert_eq!(recursive_glob.total, 2);
        assert!(!recursive_glob.scan_capped);

        let shallow_glob = glob(&pool, id, dir, "*.rs", 1, 50).await.expect("glob should succeed");
        assert_eq!(shallow_glob.paths, vec![format!("{dir}/one.rs")], "a bare * shouldn't cross into nested/ the way ** does");

        let grep_result = grep(&pool, id, dir, "fn ", None, false, 1, 20).await.expect("grep should succeed");
        let mut grep_paths: Vec<String> = grep_result.matches.iter().map(|m| m.path.clone()).collect();
        grep_paths.sort();
        assert_eq!(
            grep_paths,
            vec![format!("{dir}/nested/two.rs"), format!("{dir}/one.rs")],
            "grep should find matches across every file under path, not just the top level"
        );
        assert_eq!(grep_result.total, 2);
        assert!(!grep_result.scan_capped);
        assert!(grep_result.skipped.is_empty());

        let filtered_grep = grep(&pool, id, dir, "fn ", Some("nested/*.rs".to_string()), false, 1, 20)
            .await
            .expect("grep should succeed");
        assert_eq!(filtered_grep.matches.len(), 1, "grep's glob filter should narrow the search to just the matching file");
        assert_eq!(filtered_grep.matches[0].path, format!("{dir}/nested/two.rs"));
    })
    .await;
}
