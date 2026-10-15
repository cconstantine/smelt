use super::*;

/// A pod gets the user's SSH keys and commit identity where ssh and git
/// read them, and a reinstall after a key is deleted removes it
/// (SME-32).
#[tokio::test]
async fn test_git_files_reach_ssh_and_git_in_a_real_pod() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let sandbox = manager
        .create(&unique_session_id("git-files"), "256Mi", &[])
        .await
        .expect("create pod");
    let shell = sandbox.shell();

    let checks = tokio::time::timeout(Duration::from_secs(120), async {
        let key = crate::git::generate_key("test-key");
        let identity = crate::git::GitIdentity {
            name: "Ada \"Countess\" Lovelace".to_string(),
            email: "ada@example.com".to_string(),
        };
        let files =
            crate::git::pod_git_files(&[("test-key".to_string(), key.private_key.clone())], &identity);
        crate::git::install_git_files(&shell, &files)
            .await
            .expect("install git files");

        let mode = sandbox
            .exec(&["stat", "-c", "%a %U", "/etc/smelt/keys/test-key"])
            .await
            .expect("exec stat");
        assert_eq!(mode.stdout.trim(), "600 sandbox", "key file mode and owner");

        // The file is the key, byte for byte: ssh derives the same public half.
        let derived = sandbox
            .exec(&["ssh-keygen", "-y", "-f", "/etc/smelt/keys/test-key"])
            .await
            .expect("exec ssh-keygen");
        let expected: Vec<&str> = key.public_key.split(' ').take(2).collect();
        let got: Vec<&str> = derived.stdout.trim().split(' ').take(2).collect();
        assert_eq!(got, expected, "derived public key");

        let ssh = sandbox
            .exec(&["ssh", "-G", "github.com"])
            .await
            .expect("exec ssh -G");
        assert!(
            ssh.stdout.contains("identityfile /etc/smelt/keys/test-key"),
            "ssh -G github.com: {}",
            ssh.stdout
        );
        let known = sandbox
            .exec(&["ssh-keygen", "-F", "github.com", "-f", "/etc/ssh/ssh_known_hosts"])
            .await
            .expect("exec ssh-keygen -F");
        assert_eq!(known.exit_code, 0, "github.com is a known host: {}", known.stdout);

        let name = sandbox
            .exec(&["git", "config", "user.name"])
            .await
            .expect("exec git config");
        assert_eq!(name.stdout.trim(), "Ada \"Countess\" Lovelace");

        // Reinstalling (a key added elsewhere, the identity saved)
        // never leaves the key missing, even for a moment: a push
        // running then would fail (SME-32 code review 2, finding 5).
        let watch = sandbox.exec(&[
            "sh",
            "-c",
            "for i in $(seq 1 300); do [ -e /etc/smelt/keys/test-key ] || echo missing; sleep 0.01; done",
        ]);
        let reinstall = async {
            for _ in 0..3 {
                crate::git::install_git_files(&shell, &files).await.expect("reinstall");
            }
        };
        let (watched, ()) = tokio::join!(watch, reinstall);
        let watched = watched.expect("exec watch");
        assert!(!watched.stdout.contains("missing"), "the key vanished during a reinstall");

        // A write that fails says why (SME-32 code review 7, finding 5).
        let locked = sandbox.exec(&["chmod", "500", "/etc/smelt/keys"]).await.expect("exec chmod");
        assert_eq!(locked.exit_code, 0);
        let failed = crate::git::install_git_files(&shell, &files).await.expect_err("keys dir not writable");
        assert!(failed.to_string().contains("Permission denied"), "{failed}");
        let unlocked = sandbox.exec(&["chmod", "700", "/etc/smelt/keys"]).await.expect("exec chmod");
        assert_eq!(unlocked.exit_code, 0);

        // The key is deleted: a reinstall without it removes the file.
        let files = crate::git::pod_git_files(&[], &identity);
        crate::git::install_git_files(&shell, &files)
            .await
            .expect("reinstall git files");
        let gone = sandbox
            .exec(&["test", "-e", "/etc/smelt/keys/test-key"])
            .await
            .expect("exec test");
        assert_eq!(gone.exit_code, 1, "a deleted key's file is removed");
        let ssh = sandbox
            .exec(&["ssh", "-G", "github.com"])
            .await
            .expect("exec ssh -G");
        assert!(!ssh.stdout.contains("/etc/smelt/keys/"), "{}", ssh.stdout);
    })
    .await;

    manager.delete(sandbox).await.expect("delete pod");
    checks.expect("checks finished within the timeout");
}

/// Sets up a bare repo at /tmp/origin.git inside `sandbox`'s pod, with
/// `main` holding an AGENTS.md and a `feature` branch on top. Returns
/// main's commit.
async fn make_origin_repo(sandbox: &Sandbox) -> String {
    let script = r#"set -e
            git init -q -b main /tmp/src && cd /tmp/src
            printf 'Run make test before committing.\n' > AGENTS.md
            git add AGENTS.md && git -c user.name=t -c user.email=t@t commit -qm first
            git branch feature && git checkout -q feature
            printf 'x\n' > feature.txt && mkdir web && printf 'Use pnpm.\n' > web/AGENTS.md
            git add feature.txt web/AGENTS.md
            git -c user.name=t -c user.email=t@t commit -qm feature && git checkout -q main
            git clone -q --bare /tmp/src /tmp/origin.git
            git rev-parse main"#;
    let made = sandbox.exec(&["sh", "-c", script]).await.expect("exec make origin");
    assert_eq!(made.exit_code, 0, "make origin repo: {}", made.stdout);
    made.stdout.trim().to_string()
}

#[tokio::test]
async fn test_clone_into_pod_checks_out_a_branch_and_reports_failures() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let sandbox = manager
        .create(&unique_session_id("git-clone"), "256Mi", &[])
        .await
        .expect("create pod");
    let shell = sandbox.shell();

    let checks = tokio::time::timeout(Duration::from_secs(120), async {
        let main_commit = make_origin_repo(&sandbox).await;

        let cloned = crate::git::clone_into_pod(&shell, "file:///tmp/origin.git", None, "origin")
            .await
            .expect("clone the default branch");
        assert_eq!(cloned.commit.as_deref(), Some(main_commit.as_str()));
        assert_eq!(cloned.branch, "main");
        let agents = sandbox
            .exec(&["cat", "/workspace/origin/AGENTS.md"])
            .await
            .expect("exec cat");
        assert_eq!(agents.stdout, "Run make test before committing.\n");

        let read = crate::git::read_instructions_file(&shell, "/workspace/origin", "/workspace/origin/AGENTS.md")
            .await
            .expect("read AGENTS.md")
            .expect("origin has an AGENTS.md");
        assert_eq!(read.content, "Run make test before committing.\n");
        assert_eq!(read.file_bytes, 33);
        assert_eq!(read.hash.len(), 64, "sha256 hex: {}", read.hash);
        let listed = crate::git::list_agents_files(&shell, "origin").await.expect("list");
        assert_eq!(listed, vec!["AGENTS.md".to_string()], "main has no nested files");

        let feature = crate::git::clone_into_pod(
            &shell,
            "file:///tmp/origin.git",
            Some("feature"),
            "origin-feature",
        )
        .await
        .expect("clone a branch");
        assert_eq!(feature.branch, "feature");
        assert_ne!(feature.commit.as_deref(), Some(main_commit.as_str()));
        let listed = crate::git::list_agents_files(&shell, "origin-feature").await.expect("list");
        assert_eq!(listed, vec!["AGENTS.md".to_string(), "web/AGENTS.md".to_string()], "top-level first");

        let bare = sandbox
            .exec(&["git", "init", "-q", "/workspace/no-agents"])
            .await
            .expect("exec git init");
        assert_eq!(bare.exit_code, 0);
        let none = crate::git::read_instructions_file(&shell, "/workspace/no-agents", "/workspace/no-agents/AGENTS.md")
            .await
            .expect("read a missing file");
        assert_eq!(none, None);
        assert!(crate::git::list_agents_files(&shell, "no-agents").await.expect("list").is_empty());

        // Bytes that aren't UTF-8 (or a 1 MiB cut through a character)
        // still load, with the bad bytes replaced (SME-32 code review,
        // finding 6).
        let bad = sandbox
            .exec(&["sh", "-c", "git init -q /workspace/bad-bytes && printf 'Use \\377 tabs.\\n' > /workspace/bad-bytes/AGENTS.md"])
            .await
            .expect("exec make bad bytes");
        assert_eq!(bad.exit_code, 0, "{}", bad.stderr);
        let read = crate::git::read_instructions_file(&shell, "/workspace/bad-bytes", "/workspace/bad-bytes/AGENTS.md")
            .await
            .expect("a file with invalid UTF-8 reads")
            .expect("it exists");
        assert_eq!(read.content, "Use \u{FFFD} tabs.\n");
        assert_eq!(read.file_bytes, 12);

        // Invalid bytes decode to 3-byte replacement characters: a file
        // well under 32 KiB can decode to more, and must load whole, not
        // be cut again (SME-32 code review 8, finding 3).
        let swollen = sandbox
            .exec(&["sh", "-c", "git init -q /workspace/swollen && head -c 16000 /dev/zero | tr '\\0' '\\377' > /workspace/swollen/AGENTS.md && echo END >> /workspace/swollen/AGENTS.md"])
            .await
            .expect("exec make swollen");
        assert_eq!(swollen.exit_code, 0, "{}", swollen.stderr);
        let read = crate::git::read_instructions_file(&shell, "/workspace/swollen", "/workspace/swollen/AGENTS.md")
            .await
            .expect("read")
            .expect("it exists");
        assert_eq!(read.file_bytes, 16004);
        assert!(read.content.ends_with("END\n"), "the whole file loads: ...{:?}", read.content.chars().rev().take(5).collect::<String>());

        // A non-ASCII path is listed as it is, not in git's quoting,
        // so it can be loaded (SME-32 code review 7, finding 3).
        let unicode = sandbox
            .exec(&["sh", "-c", "git init -q /workspace/unicode && cd /workspace/unicode && mkdir é && echo x > é/AGENTS.md && git add é/AGENTS.md"])
            .await
            .expect("exec make unicode");
        assert_eq!(unicode.exit_code, 0, "{}", unicode.stderr);
        let listed = crate::git::list_agents_files(&shell, "unicode").await.expect("list");
        assert_eq!(listed, vec!["é/AGENTS.md".to_string()]);

        // An AGENTS.md that's a symlink out of the checkout (to a key,
        // say) is refused, not read (SME-32 code review 7, finding 1).
        let linked = sandbox
            .exec(&["sh", "-c", "git init -q /workspace/linked && ln -s /etc/passwd /workspace/linked/AGENTS.md"])
            .await
            .expect("exec make symlink");
        assert_eq!(linked.exit_code, 0, "{}", linked.stderr);
        let refused = crate::git::read_instructions_file(&shell, "/workspace/linked", "/workspace/linked/AGENTS.md")
            .await
            .expect_err("a symlink isn't read");
        assert!(refused.contains("isn't a regular file"), "{refused}");
        assert!(!refused.contains("root:"), "nothing of the target leaks: {refused}");

        let missing = crate::git::clone_into_pod(&shell, "file:///tmp/nope.git", None, "nope")
            .await
            .expect_err("a missing repo fails");
        assert!(missing.contains("does not appear to be a git repository"), "{missing}");

        // The directory is taken: git's own message, not a silent overwrite.
        let taken = crate::git::clone_into_pod(&shell, "file:///tmp/origin.git", None, "origin")
            .await
            .expect_err("an existing directory fails");
        assert!(taken.contains("already exists"), "{taken}");
        // Git refuses an existing directory; the error says what to do
        // when it's what a cut-off clone left (SME-32 code review 5).
        assert!(taken.contains("delete it"), "{taken}");
        let untouched = sandbox
            .exec(&["cat", "/workspace/origin/AGENTS.md"])
            .await
            .expect("exec cat");
        assert_eq!(untouched.stdout, "Run make test before committing.\n", "the existing checkout is left alone");
    })
    .await;

    manager.delete(sandbox).await.expect("delete pod");
    checks.expect("checks finished within the timeout");
}

#[test]
fn test_exec_with_no_status_is_a_failure_not_a_success() {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;
    // The connection closed before the API server said how it ended.
    assert_eq!(extract_exit_code(None), -1);
    // What a successful exec actually ends with: no causes at all.
    let success = Status {
        status: Some("Success".to_string()),
        ..Default::default()
    };
    assert_eq!(extract_exit_code(Some(success)), 0);
}
