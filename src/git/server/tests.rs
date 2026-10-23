use super::*;

/// Only a missing pod is "no sandbox yet"; anything else says what
/// it is, so the model isn't sent to create_pod for nothing (SME-56
/// code review 1).
#[test]
fn test_no_shell_message_names_the_error_unless_there_is_no_pod() {
    assert_eq!(
        no_shell_message(sandbox::TerminalError::NoPod),
        "This conversation has no sandbox yet: call create_pod first."
    );
    let not_set_up = no_shell_message(sandbox::TerminalError::Sandbox(sandbox::SandboxError::NotInitialized));
    assert!(!not_set_up.contains("create_pod"), "{not_set_up}");
    assert!(not_set_up.contains("isn't set up"), "{not_set_up}");
}

#[tokio::test]
async fn test_an_install_that_read_old_keys_cant_write_over_a_newer_one() {
    let installed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    // A reads the keys first but slowly; B reads the newer ones
    // after it started, and quickly.
    let a = {
        let installed = installed.clone();
        tokio::spawn(with_install_lock(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            installed.lock().expect("log").push("old");
        }))
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let b = {
        let installed = installed.clone();
        tokio::spawn(with_install_lock(async move {
            installed.lock().expect("log").push("new");
        }))
    };
    a.await.expect("a");
    b.await.expect("b");
    assert_eq!(installed.lock().expect("log").last(), Some(&"new"), "the pod ends with the newest keys");
}

fn instructions(path: &str, content: &str, file_bytes: u64) -> ProjectInstructions {
    ProjectInstructions {
        repo_url: "git@github.com:o/smelt.git".to_string(),
        path: path.to_string(),
        commit: Some("43835b44f939c268b73b49292428911526a51508".to_string()),
        content: content.to_string(),
        file_bytes,
        truncated: file_bytes > INSTRUCTIONS_MAX_BYTES as u64,
    }
}

#[test]
fn test_project_instructions_render_each_file_with_where_it_came_from() {
    let rendered = render_project_instructions(&[
        instructions("/workspace/smelt/AGENTS.md", "Run make test.\n", 15),
        instructions("/workspace/smelt/web/AGENTS.md", "Use pnpm.", 9),
    ]);
    assert!(rendered.starts_with("\n# Project instructions\n"), "{rendered}");
    assert!(rendered.contains("user's own messages take precedence"), "{rendered}");
    assert!(rendered.contains("nearest"), "{rendered}");
    let top = rendered
        .find("## /workspace/smelt/AGENTS.md (git@github.com:o/smelt.git at 43835b4)\n\nRun make test.\n")
        .expect("top-level section");
    let web = rendered.find("## /workspace/smelt/web/AGENTS.md").expect("nested section");
    assert!(top < web);
    assert!(rendered.contains("Use pnpm.\n"), "a missing final newline is added: {rendered}");
    assert!(!rendered.contains("Truncated"), "{rendered}");
}

#[test]
fn test_project_instructions_say_when_a_file_was_cut() {
    let rendered = render_project_instructions(&[instructions("/workspace/big/AGENTS.md", "start", 50_000)]);
    assert!(
        rendered.contains("Truncated: this file is 50000 bytes; only the first 32768 are here. Read the rest with read_file."),
        "{rendered}"
    );
}

#[test]
fn test_no_project_instructions_render_nothing() {
    assert_eq!(render_project_instructions(&[]), "");
}

fn repo_in(dir: &str) -> db::ConversationRepo {
    db::ConversationRepo {
        id: dir.len() as i64,
        conversation_id: 1,
        url: format!("git@github.com:o/{dir}.git"),
        remote_key: format!("github.com/o/{dir}"),
        branch: None,
        dir: dir.to_string(),
        status: "ready".to_string(),
        error: None,
        checked_out_branch: None,
        commit_sha: None,
        created_at: chrono::DateTime::from_timestamp(0, 0).expect("epoch").naive_utc(),
        updated_at: chrono::DateTime::from_timestamp(0, 0).expect("epoch").naive_utc(),
        agents_files: vec![],
        attempt: 1,
        persona_files: vec![],
    }
}

#[test]
fn test_an_agents_md_path_resolves_to_its_repo() {
    let repos = vec![repo_in("smelt"), repo_in("docs")];
    let (repo, rel) = resolve_instructions_path(&repos, "/workspace/smelt/AGENTS.md").expect("top-level");
    assert_eq!((repo.dir.as_str(), rel.as_str()), ("smelt", "AGENTS.md"));
    let (repo, rel) = resolve_instructions_path(&repos, "/workspace/docs/guide/AGENTS.md").expect("nested");
    assert_eq!((repo.dir.as_str(), rel.as_str()), ("docs", "guide/AGENTS.md"));
    for bad in [
        "/workspace/other/AGENTS.md",
        "/workspace/smelt/README.md",
        "/workspace/smelt/../docs/AGENTS.md",
        "/workspace/smelt/./AGENTS.md",
        "smelt/AGENTS.md",
        "/workspace/smelt",
        "/etc/AGENTS.md",
    ] {
        assert!(resolve_instructions_path(&repos, bad).is_err(), "{bad}");
    }
    // A path that can break or reorder a line is refused before any read
    // or trust request, so no trust card shows it, and the refusal quotes
    // it (SME-271); a non-ASCII one still resolves.
    for bad in [
        "/workspace/smelt/x\n- y/AGENTS.md",
        "/workspace/smelt/a\u{2028}b/AGENTS.md",
        "/workspace/smelt/a\u{202e}b/AGENTS.md",
        // SME-276: characters that render as nothing.
        "/workspace/smelt/a\u{200b}b/AGENTS.md",
        "/workspace/smelt/a\u{e0041}\u{e0042}b/AGENTS.md",
        // Review 1: a Hangul filler is a letter to Rust's Debug, which
        // left it raw in the refusal.
        "/workspace/smelt/a\u{3164}b/AGENTS.md",
    ] {
        let refused = resolve_instructions_path(&repos, bad).expect_err(bad);
        assert!(refused.contains("control characters"), "{refused}");
        assert!(!refused.contains(['\n', '\u{2028}', '\u{202e}', '\u{200b}', '\u{e0041}', '\u{3164}']), "quoted: {refused}");
    }
    let (_, rel) = resolve_instructions_path(&repos, "/workspace/smelt/é/AGENTS.md").expect("non-ASCII");
    assert_eq!(rel, "é/AGENTS.md");
}

/// "Cut" is about the file's size, not its decoded length, which
/// invalid bytes inflate (SME-32 code review 9, finding 3).
#[test]
fn test_the_model_is_told_when_a_loaded_file_was_cut() {
    let swollen = db::InstructionsFile {
        content: "\u{FFFD}".repeat(32_000),
        file_bytes: 40_000,
        hash: "h".to_string(),
        commit: None,
    };
    assert!(loaded_message("/workspace/r/AGENTS.md", &swollen).contains("cut to 32 KiB"));
    let whole = db::InstructionsFile {
        content: "short".to_string(),
        file_bytes: 5,
        hash: "h".to_string(),
        commit: None,
    };
    assert!(!loaded_message("/workspace/r/AGENTS.md", &whole).contains("cut"));
}

#[test]
fn test_a_failed_clone_says_what_happened() {
    let url = "git@github.com:o/r.git";
    let path = "/workspace/r";
    // Only 124 is the pod's `timeout` stopping git.
    assert!(clone_failure(124, "", url, path).starts_with(CLONE_TIMED_OUT));
    // 137 is SIGKILL, from `timeout` or not: often the memory limit.
    let killed = clone_failure(137, "", url, path);
    assert!(!killed.starts_with(CLONE_TIMED_OUT), "{killed}");
    assert!(killed.contains("killed") && killed.contains("memory"), "{killed}");
    // Git's own words otherwise, with a hint for an existing directory.
    assert_eq!(clone_failure(128, "fatal: Remote branch nope not found\n", url, path), "fatal: Remote branch nope not found");
    let exists = clone_failure(128, "fatal: destination path '/workspace/r' already exists and is not an empty directory.", url, path);
    assert!(exists.contains("rm -rf /workspace/r"), "{exists}");
    assert_eq!(clone_failure(2, "", url, path), "git clone git@github.com:o/r.git failed (exit code 2)");
}

#[test]
fn test_the_clone_runs_under_the_pods_own_timeout() {
    let command = clone_command("git@github.com:o/r.git", Some("dev"), "/workspace/r");
    let secs = CLONE_TIMEOUT.as_secs().to_string();
    assert_eq!(
        command,
        vec![
            "env", "GIT_TERMINAL_PROMPT=0", "timeout", "--kill-after=10", &secs, "git", "-c",
            "core.sshCommand=ssh -o BatchMode=yes", "clone", "--quiet", "--branch", "dev", "--",
            "git@github.com:o/r.git", "/workspace/r",
        ]
    );
    assert!(!clone_command("u", None, "/workspace/r").contains(&"--branch".to_string()));
}

#[test]
fn test_remote_key_is_the_same_for_every_url_form() {
    for url in [
        "git@github.com:Owner/Repo.git",
        "ssh://git@github.com/owner/repo",
        "ssh://git@github.com:22/owner/repo.git",
        "https://github.com/owner/repo.git",
        "https://github.com/OWNER/repo/",
        "https://user@github.com/owner/repo",
        "http://github.com/owner/repo",
        "git://github.com/owner/repo.git",
    ] {
        assert_eq!(remote_key(url).as_deref(), Some("github.com/owner/repo"), "{url}");
    }
    assert_eq!(
        remote_key("https://gitlab.com/group/sub/project.git").as_deref(),
        Some("gitlab.com/group/sub/project")
    );
    // A local path in the pod (tests use bare repos this way).
    assert_eq!(remote_key("file:///tmp/origin.git").as_deref(), Some("file/tmp/origin"));
}

/// Case only folds where the host ignores it: elsewhere two repos
/// differing in case are two repos (SME-32 code review 7, finding 6).
#[test]
fn test_remote_key_keeps_path_case_on_other_hosts() {
    assert_eq!(remote_key("git@GitLab.com:Group/Project.git").as_deref(), Some("gitlab.com/group/project"));
    assert_eq!(remote_key("https://Bitbucket.org/Team/Repo").as_deref(), Some("bitbucket.org/team/repo"));
    assert_eq!(remote_key("git@git.Example.com:Team/Repo.git").as_deref(), Some("git.example.com/Team/Repo"));
    assert_ne!(remote_key("git@example.com:Team/Repo"), remote_key("git@example.com:team/repo"));
    assert_eq!(remote_key("file:///tmp/Origin.git").as_deref(), Some("file/tmp/Origin"));
}

#[test]
fn test_remote_key_refuses_what_isnt_a_url() {
    for bad in ["", "   ", "github.com", "owner/repo", "https://", "https://github.com", "-u evil", "ext::sh -c touch% /tmp/x"] {
        assert_eq!(remote_key(bad), None, "{bad:?}");
    }
}

#[test]
fn test_default_checkout_dir_is_the_repo_name() {
    assert_eq!(default_checkout_dir("git@github.com:o/smelt.git").as_deref(), Some("smelt"));
    assert_eq!(default_checkout_dir("https://github.com/o/Smelt/").as_deref(), Some("Smelt"));
    assert_eq!(default_checkout_dir("file:///tmp/origin.git").as_deref(), Some("origin"));
    assert_eq!(default_checkout_dir("owner/repo"), None);
}

#[test]
fn test_generated_key_is_ed25519_and_its_halves_match() {
    let pair = generate_key("laptop");
    let private = ssh_key::PrivateKey::from_openssh(&pair.private_key)
        .expect("the private key should parse");
    assert_eq!(private.algorithm(), ssh_key::Algorithm::Ed25519);
    assert_eq!(
        private.public_key().to_openssh().expect("encode"),
        pair.public_key
    );
    assert!(pair.public_key.ends_with(" smelt:laptop"), "{}", pair.public_key);
}

#[test]
fn test_two_generated_keys_differ() {
    assert_ne!(generate_key("a").private_key, generate_key("a").private_key);
}

#[test]
fn test_import_derives_the_public_key() {
    let pair = generate_key("imported");
    let imported = import_key(&pair.private_key).expect("a valid key imports");
    assert_eq!(imported.public_key, pair.public_key);
    assert_eq!(imported.private_key, pair.private_key);
}

#[test]
fn test_import_accepts_surrounding_whitespace_and_crlf() {
    let pair = generate_key("pasted");
    let pasted = format!("\n  {}  \n", pair.private_key.replace('\n', "\r\n"));
    let imported = import_key(&pasted).expect("a pasted key imports");
    assert_eq!(imported.public_key, pair.public_key);
}

#[test]
fn test_import_refuses_garbage_with_a_readable_message() {
    let err = import_key("ssh-ed25519 AAAA… this is a public key").expect_err("refused");
    assert!(err.contains("private key"), "{err}");
}

#[test]
fn test_import_refuses_a_passphrase_protected_key() {
    // Made with `ssh-keygen -t ed25519 -N secret -C test`.
    let err = import_key(ENCRYPTED_KEY).expect_err("refused");
    assert!(err.contains("passphrase"), "{err}");
}

#[test]
fn test_fingerprint_matches_ssh_keygen() {
    // `ssh-keygen -lf` on ENCRYPTED_KEY's public half.
    assert_eq!(
        fingerprint(ENCRYPTED_KEY_PUB).expect("parses"),
        ENCRYPTED_KEY_FINGERPRINT
    );
}

#[test]
fn test_key_names_are_file_name_safe() {
    for ok in ["github", "work-laptop", "deploy_smelt", "A1"] {
        assert!(validate_key_name(ok).is_ok(), "{ok}");
    }
    for bad in ["", "has space", "../escape", "a/b", "ünïcode", &"x".repeat(65)] {
        assert!(validate_key_name(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn test_identity_refuses_newlines() {
    let ok = GitIdentity {
        name: "Ada Lovelace".into(),
        email: "ada@example.com".into(),
    };
    assert!(validate_identity(&ok).is_ok());
    let bad = GitIdentity {
        name: "Ada\n[core]\n\tsshCommand = evil".into(),
        email: "ada@example.com".into(),
    };
    assert!(validate_identity(&bad).is_err());
}

fn file<'a>(files: &'a [PodFile], path: &str) -> Option<&'a PodFile> {
    files.iter().find(|f| f.path == path)
}

#[test]
fn test_pod_files_hold_each_key_privately_and_offer_every_one() {
    let keys = vec![
        ("github".to_string(), "PRIVATE-1".to_string()),
        ("work".to_string(), "PRIVATE-2".to_string()),
    ];
    let files = pod_git_files(&keys, &GitIdentity::default());

    let github = file(&files, "/etc/smelt/keys/github").expect("key file");
    assert_eq!(github.content, "PRIVATE-1\n");
    assert_eq!(github.mode, 0o600);
    assert_eq!(file(&files, "/etc/smelt/keys/work").expect("key").content, "PRIVATE-2\n");

    let config = file(&files, "/etc/smelt/ssh_config").expect("ssh config");
    assert!(config.content.contains("IdentityFile /etc/smelt/keys/github\n"), "{}", config.content);
    assert!(config.content.contains("IdentityFile /etc/smelt/keys/work\n"), "{}", config.content);
    // Hosts not in the image's known_hosts are accepted the first
    // time, not refused with a prompt nobody can answer.
    assert!(config.content.contains("StrictHostKeyChecking accept-new"), "{}", config.content);
}

#[test]
fn test_pod_files_quote_the_identity_for_git_config() {
    let identity = GitIdentity {
        name: r#"Ada "The Countess" Lovelace\"#.into(),
        email: "ada@example.com".into(),
    };
    let files = pod_git_files(&[], &identity);
    let config = file(&files, "/etc/smelt/gitconfig").expect("gitconfig");
    assert_eq!(
        config.content,
        "[user]\n\tname = \"Ada \\\"The Countess\\\" Lovelace\\\\\"\n\temail = \"ada@example.com\"\n"
    );
}

#[test]
fn test_pod_files_leave_the_identity_empty_when_unset() {
    let files = pod_git_files(&[], &GitIdentity::default());
    let config = file(&files, "/etc/smelt/gitconfig").expect("gitconfig is always written");
    assert_eq!(config.content, "");
    // No keys: the SSH config still exists, offering none.
    let ssh = file(&files, "/etc/smelt/ssh_config").expect("ssh config");
    assert!(!ssh.content.contains("IdentityFile"), "{}", ssh.content);
}

const ENCRYPTED_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABB0Ht94ps
1pc8RavlT/LsnzAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIF3I8kAQGuwp+D3C
ue9eeOLmUaTpNtML5gyjN0/jzbdFAAAAkD0ecctju+ef8Bx/ZuprZrtEAA0QDSHEgPw02S
r1/jznE5AU9c9fCQlvapkCYD2SZ0PDjF4OzRqdu5GA3mx6L6hwOtB2q9IbXhG09Pemnx+N
dIwV1foqIg1UBR+irWu2+JpBxBBTWoW4q5Ckq6HA2qM8p4J5V0cNqsKB9dUSZlxMu4EG+O
TeG9b4Wi3JZlayBA==
-----END OPENSSH PRIVATE KEY-----
";
const ENCRYPTED_KEY_PUB: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIF3I8kAQGuwp+D3Cue9eeOLmUaTpNtML5gyjN0/jzbdF test";
const ENCRYPTED_KEY_FINGERPRINT: &str = "SHA256:oYEa62H9Lg8UaDiZ0jy+2GXNR5OMjJ4DEfTZcZ0ncuM";
