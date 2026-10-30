use super::*;

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
