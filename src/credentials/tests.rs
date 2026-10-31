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

// --- T1 (SME-242): both rules, read through `for_conversation` ---

async fn new_conversation(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title) VALUES ('t') RETURNING id")
        .fetch_one(pool)
        .await
        .expect("a conversation")
}

/// An `active` agent row: `credentials` is the chain column (`None`
/// leaves it NULL, the pre-ticket shape), `snapshot` the persona
/// snapshot's `credentials` list (`None` leaves no snapshot at all).
async fn agent(
    pool: &PgPool,
    credentials: Option<&[&str]>,
    snapshot: Option<&[&str]>,
) -> i64 {
    let conversation = new_conversation(pool).await;
    let credentials: Option<Vec<String>> = credentials.map(|l| l.iter().map(|n| n.to_string()).collect());
    let settings = snapshot.map(|names| serde_json::json!({ "credentials": names }));
    sqlx::query(
        "INSERT INTO agents (conversation_id, root_id, depth, brief, state, credentials, persona_settings)
         VALUES ($1, $1, 1, 'b', 'active', $2, $3)",
    )
    .bind(conversation)
    .bind(credentials)
    .bind(settings)
    .execute(pool)
    .await
    .expect("an agent row");
    conversation
}

async fn stored_secret(pool: &PgPool, name: &str) {
    sqlx::query("INSERT INTO named_secrets (name, value) VALUES ($1, 'v')")
        .bind(name)
        .execute(pool)
        .await
        .expect("a secret");
}

/// Rule G's root arm: the user's own conversation has the key iff one
/// is stored — and never a named secret, whoever stores one (the root's
/// pod holding no secret file is the design's headline trade-off).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_root_gets_the_key_when_one_is_stored_and_never_a_secret(pool: PgPool) {
    let root = new_conversation(&pool).await;
    let decided = || for_conversation(&pool, root);
    assert_eq!(decided().await.expect("rule reads"), PodCredentials { git_ssh: false, secrets: vec![] });
    db::create_ssh_key(&pool, "id_ed25519", "pub", "priv").await.expect("a key");
    stored_secret(&pool, "linear-key").await;
    stored_secret(&pool, "grafana").await;
    assert_eq!(
        decided().await.expect("rule reads"),
        PodCredentials { git_ssh: true, secrets: vec![] },
        "the key yes, the store's names no — Rule S asks a persona, and a root has none"
    );
}

/// Rule G's agent arms: the bit nests, an empty bit denies, and a NULL
/// column — only ever a pre-ticket row — falls open to granted.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_agents_chain_bit_decides_git_ssh_and_null_falls_open(pool: PgPool) {
    let holder = agent(&pool, Some(&["git-ssh"]), None).await;
    assert!(for_conversation(&pool, holder).await.expect("rule reads").git_ssh, "the parent's bit, kept");
    let narrowed = agent(&pool, Some(&[]), None).await;
    assert!(!for_conversation(&pool, narrowed).await.expect("rule reads").git_ssh, "a narrowed bit stays narrowed");
    let legacy = agent(&pool, None, None).await;
    assert!(for_conversation(&pool, legacy).await.expect("rule reads").git_ssh, "NULL falls open, even with no key stored");
}

/// Rule S, the exempt rule: this conversation's own persona names the
/// secret and the store backs it — whatever its chain holds. A persona-
/// less agent gets no secrets at all, only the chain bit it was seeded
/// with; the empty list grants neither kind.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_persona_secret_is_granted_whatever_its_chain_holds(pool: PgPool) {
    stored_secret(&pool, "deep-key").await;
    let holder = agent(&pool, Some(&["git-ssh"]), Some(&["deep-key"])).await;
    assert_eq!(for_conversation(&pool, holder).await.expect("rule reads"), PodCredentials {
        git_ssh: true,
        secrets: vec!["deep-key".to_string()],
    });
    // The T13 case at rule level: a chain that holds nothing (the
    // column narrowed away at each prepare) still gets the secret this
    // agent's own persona names — no ancestor ever held it.
    let deep = agent(&pool, Some(&[]), Some(&["deep-key"])).await;
    assert_eq!(for_conversation(&pool, deep).await.expect("rule reads"), PodCredentials {
        git_ssh: false,
        secrets: vec!["deep-key".to_string()],
    });
    // No persona: no secrets, just the parent's bit it was seeded with.
    let plain = agent(&pool, Some(&["git-ssh"]), None).await;
    assert_eq!(for_conversation(&pool, plain).await.expect("rule reads"), PodCredentials {
        git_ssh: true,
        secrets: vec![],
    });
    // `credentials: []` — narrowed away — grants neither kind.
    let empty = agent(&pool, Some(&[]), Some(&[])).await;
    assert_eq!(for_conversation(&pool, empty).await.expect("rule reads"), PodCredentials {
        git_ssh: false,
        secrets: vec![],
    });
    // A name the store doesn't back grants nothing (D6 reports it).
    let unbacked = agent(&pool, Some(&[]), Some(&["nope"])).await;
    assert_eq!(for_conversation(&pool, unbacked).await.expect("rule reads"), PodCredentials {
        git_ssh: false,
        secrets: vec![],
    });
}

/// `pod_files` is the rules' file form: keys only under Rule G (no key
/// file, no `IdentityFile` line, for a narrowed chain), secrets always
/// 600 and named exactly as stored, and the config files either way.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_pod_files_follow_the_two_rules_separately(pool: PgPool) {
    db::create_ssh_key(&pool, "id_ed25519", "pub", "priv").await.expect("a key");
    stored_secret(&pool, "linear-key").await;
    let granted = agent(&pool, Some(&["git-ssh"]), Some(&["linear-key"])).await;
    let files = pod_files(&pool, granted).await.expect("pod files");
    assert_eq!(file(&files, "/etc/smelt/keys/id_ed25519").expect("the key file").mode, 0o600);
    let secret = file(&files, "/etc/smelt/secrets/linear-key").expect("the secret file");
    assert_eq!((secret.mode, secret.content.as_str()), (0o600, "v"));
    // The same store, a chain narrowed away: no key file, no offered
    // IdentityFile — but its persona's secret still lands.
    let narrowed = agent(&pool, Some(&[]), Some(&["linear-key"])).await;
    let files = pod_files(&pool, narrowed).await.expect("pod files");
    assert!(file(&files, "/etc/smelt/keys/id_ed25519").is_none(), "Rule G says no");
    let ssh = file(&files, "/etc/smelt/ssh_config").expect("ssh config");
    assert!(!ssh.content.contains("IdentityFile"), "{}", ssh.content);
    assert!(file(&files, "/etc/smelt/secrets/linear-key").is_some(), "Rule S says yes");
    // A root conversation with the same store: the key, no secrets.
    let root = new_conversation(&pool).await;
    let files = pod_files(&pool, root).await.expect("pod files");
    assert!(file(&files, "/etc/smelt/keys/id_ed25519").is_some());
    assert!(file(&files, "/etc/smelt/secrets/linear-key").is_none());
    assert!(file(&files, "/etc/smelt/gitconfig").is_some(), "the identity file is always written");
}

/// SME-334 T5: the store → serve pin. A stored multi-line secret
/// reaches `PodFile.content` byte-equal, at mode 600 — saved through
/// the real save path, so the tdd.md stub check bites here too: stub
/// `create_named_secret` to flatten newlines and this row's content
/// goes red. `install_pod_files`' `cat > "$1.new"` write has been
/// byte-faithful since SME-242; with T3 and T4 this pins save → store
/// → serve with no byte discretion anywhere in between.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_pod_files_hand_a_multi_line_secret_byte_equal(pool: PgPool) {
    let value = "-----BEGIN OPENSSH PRIVATE KEY-----\n\nb3BlbnNzaC1rZXktdjEA\n-----END OPENSSH PRIVATE KEY-----\n";
    crate::api::secrets::save_secret_of(&pool, None, "pem-key", value).await.expect("saved");
    let holder = agent(&pool, Some(&[]), Some(&["pem-key"])).await;
    let files = pod_files(&pool, holder).await.expect("pod files");
    let secret = file(&files, "/etc/smelt/secrets/pem-key").expect("the secret file");
    assert_eq!((secret.mode, secret.content.as_str()), (0o600, value), "byte-equal, at 600, exactly as stored");
}

