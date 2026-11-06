use super::*;

#[tokio::test]
async fn test_an_install_that_read_old_keys_cant_write_over_a_newer_one() {
    let installed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    // A reads the keys first but slowly; B reads the newer ones
    // after it started, and quickly.
    let a = {
        let installed = installed.clone();
        tokio::spawn(with_install_lock(1, async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            installed.lock().expect("log").push("old");
        }))
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let b = {
        let installed = installed.clone();
        tokio::spawn(with_install_lock(1, async move {
            installed.lock().expect("log").push("new");
        }))
    };
    a.await.expect("a");
    b.await.expect("b");
    assert_eq!(installed.lock().expect("log").last(), Some(&"new"), "the pod ends with the newest keys");
}

/// SME-341: the lock is a conversation's, so an install into another
/// conversation's pod doesn't wait for one in progress. One lock for every
/// pod made a key or secret change write to each live pod in turn.
#[tokio::test]
async fn test_an_install_into_another_conversation_doesnt_wait() {
    let (held, release) = tokio::sync::oneshot::channel::<()>();
    let (started, started_rx) = tokio::sync::oneshot::channel::<()>();
    let slow = tokio::spawn(with_install_lock(2_000_001, async move {
        let _ = started.send(());
        let _ = release.await;
    }));
    started_rx.await.expect("the first install started");
    let other = tokio::time::timeout(std::time::Duration::from_secs(5), with_install_lock(2_000_002, async { "done" })).await;
    assert_eq!(other, Ok("done"), "an install into another conversation's pod waited for the first");
    let _ = held.send(());
    slow.await.expect("the first install");
}

/// SME-371: an install that won't finish doesn't hold the answer, the
/// answer counts the pods not known written (the stuck one and the one that
/// failed, not the one that finished), and the stuck install isn't called
/// off: it lands after the answer. An answer that cancelled its installs at
/// the deadline would leave the pod with its old files until its next create.
#[tokio::test]
async fn test_the_answer_doesnt_wait_for_a_stuck_install_and_the_install_still_lands() {
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let released = std::sync::Arc::new(tokio::sync::Mutex::new(Some(released)));
    let (landed, did_land) = tokio::sync::oneshot::channel::<i64>();
    let landed = std::sync::Arc::new(std::sync::Mutex::new(Some(landed)));
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        installs_answered_within(std::time::Duration::from_millis(200), vec![1, 2, 3], move |pod_id| {
            let released = released.clone();
            let landed = landed.clone();
            async move {
                match pod_id {
                    1 => Ok(()),
                    2 => {
                        if let Some(released) = released.lock().await.take() {
                            let _ = released.await;
                        }
                        if let Some(landed) = landed.lock().expect("the landed lock").take() {
                            let _ = landed.send(pod_id);
                        }
                        Ok(())
                    }
                    _ => Err("this pod can't be reached".to_string()),
                }
            }
        }),
    )
    .await;
    let _ = release.send(());
    let mut missed = answer.expect("the answer waited for the stuck install");
    missed.sort_unstable();
    assert_eq!(missed, vec![2, 3], "the stuck and the failed install aren't known written; the finished one is");
    let landed = tokio::time::timeout(std::time::Duration::from_secs(5), did_land).await;
    assert_eq!(landed.expect("the stuck install never landed").expect("the install was called off"), 2);
}

/// SME-371 review 1: the whole claim that a late install can't bring a
/// deleted secret back rests on this. An install waiting for its
/// conversation's lock reads the pod's files only once it holds the lock, so
/// a secret deleted while it waited isn't in what it writes. Red if the read
/// moves before the lock.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_install_reads_its_files_only_once_it_holds_the_lock(pool: PgPool) {
    let saved = crate::db::create_named_secret(&pool, "linear-key", "value-1234567890").await.expect("a stored secret");
    let holder_conversation = agent(&pool, Some(&[]), Some(&["linear-key"])).await;
    let pod = crate::db::create_sandbox_pod(&pool, holder_conversation).await.expect("a live pod row");
    assert!(
        file(&pod_files(&pool, holder_conversation).await.expect("pod files"), "/etc/smelt/secrets/linear-key").is_some(),
        "the secret is granted, so its absence below is the delete's doing"
    );

    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let (is_held, held) = tokio::sync::oneshot::channel::<()>();
    let lock = tokio::spawn(with_install_lock(holder_conversation, async move {
        let _ = is_held.send(());
        let _ = released.await;
    }));
    held.await.expect("the lock is held");

    let (written, wrote) = tokio::sync::oneshot::channel::<Vec<PodFile>>();
    let install = {
        let pool = pool.clone();
        tokio::spawn(async move {
            install_into_pod_with(&pool, pod.id, |_, files| async move {
                let _ = written.send(files);
                Ok(())
            })
            .await
        })
    };
    // Let the install run up to the lock, then delete under it.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    crate::db::delete_named_secret(&pool, saved.id).await.expect("delete");
    let _ = release.send(());
    lock.await.expect("the holder");
    let files = tokio::time::timeout(std::time::Duration::from_secs(10), wrote)
        .await
        .expect("the install never wrote")
        .expect("the writer ran");
    install.await.expect("the install").expect("installed");
    assert!(file(&files, "/etc/smelt/secrets/linear-key").is_none(), "a deleted secret came back with a late install");
}

/// SME-371 reviews 1 and 2: the deadline leaves a record naming only the pods
/// still installing at it, for the browser tier's report. A scenario's own
/// hold can be told from the rest, even when both fire at once, and neither a
/// finished install nor one that failed quickly is a stall: a failure beside
/// the hold must not read as one. The answer still counts the failure, which
/// a delete has to say.
#[tokio::test]
async fn test_the_deadline_records_the_pods_it_left() {
    let held = crate::id_alloc::FIXTURE_IDS.next_base();
    let stalled = crate::id_alloc::FIXTURE_IDS.next_base();
    let finished = crate::id_alloc::FIXTURE_IDS.next_base();
    let failing = crate::id_alloc::FIXTURE_IDS.next_base();
    let (release, released) = tokio::sync::watch::channel(false);
    let answer = |pods: Vec<i64>| {
        let released = released.clone();
        async move {
            let mut answered = installs_answered_within(std::time::Duration::from_millis(50), pods, move |pod_id| {
                let mut released = released.clone();
                async move {
                    if pod_id == failing {
                        return Err("this pod can't be reached".to_string());
                    }
                    if pod_id != finished {
                        let _ = released.wait_for(|go| *go).await;
                    }
                    Ok(())
                }
            })
            .await;
            answered.sort_unstable();
            answered
        }
    };
    let sorted = |mut pods: Vec<i64>| {
        pods.sort_unstable();
        pods
    };
    let mine = |besides: &[i64]| -> Vec<Vec<i64>> {
        install_deadline_firings_besides(besides)
            .into_iter()
            .filter(|pods| pods.iter().any(|pod| [held, stalled, finished, failing].contains(pod)))
            .collect()
    };

    assert_eq!(answer(vec![held, failing]).await, sorted(vec![held, failing]), "the answer counts the failure");
    assert_eq!(mine(&[held]), Vec::<Vec<i64>>::new(), "a failure beside the hold is no stall");
    assert_eq!(mine(&[]), vec![vec![held]], "the hold itself fired, and the record holds only it");

    assert_eq!(answer(vec![held, stalled, finished, failing]).await, sorted(vec![held, stalled, failing]));
    assert_eq!(mine(&[held]), vec![vec![stalled]], "a stall beside the hold reports only itself");
    let _ = release.send(true);
}

/// Every install finished: the answer doesn't sit out the deadline, and
/// nothing is missed. The normal save, which still answers with its pods
/// written.
#[tokio::test]
async fn test_the_answer_comes_as_soon_as_every_install_finishes() {
    let missed = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        installs_answered_within(std::time::Duration::from_secs(60), vec![1, 2], |_| async { Ok(()) }),
    )
    .await;
    assert_eq!(missed.expect("the answer sat out its deadline"), Vec::<i64>::new());
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
/// → serve with no byte discretion anywhere in between. Two values:
/// the PEM ending in `\n`, and one ending mid-line — only a tailless
/// value tells a faithful serve apart from the git-key branch's own
/// append-if-missing habit (`credentials.rs`'s keys), which a `\n`-
/// ended value would let pass unnoticed.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_pod_files_hand_a_multi_line_secret_byte_equal(pool: PgPool) {
    let value = "-----BEGIN OPENSSH PRIVATE KEY-----\n\nb3BlbnNzaC1rZXktdjEA\n-----END OPENSSH PRIVATE KEY-----\n";
    let unended = "apiVersion: v1\nclusters";
    crate::api::secrets::save_secret_of(&pool, None, "pem-key", value).await.expect("saved the PEM");
    crate::api::secrets::save_secret_of(&pool, None, "no-tail", unended).await.expect("saved the tailless one");
    let holder = agent(&pool, Some(&[]), Some(&["pem-key", "no-tail"])).await;
    let files = pod_files(&pool, holder).await.expect("pod files");
    let secret = file(&files, "/etc/smelt/secrets/pem-key").expect("the secret file");
    assert_eq!((secret.mode, secret.content.as_str()), (0o600, value), "byte-equal, at 600, exactly as stored");
    let tailless = file(&files, "/etc/smelt/secrets/no-tail").expect("the tailless secret file");
    assert_eq!((tailless.mode, tailless.content.as_str()), (0o600, unended), "ends exactly where it was stored: no byte added, none dropped");
}


/// SME-440 review M2: each try of an exec gets longer, 10 s, then 20 s,
/// then what is left of the install's own bound, so an exec slower than
/// the first try's limit still finishes rather than being re-issued until
/// the install fails; and the first two leave the third room.
#[test]
fn test_an_execs_tries_grow_and_end_at_the_installs_bound() {
    let s = std::time::Duration::from_secs;
    assert_eq!(try_limit(0, INSTALL_TIMEOUT), s(10));
    assert_eq!(try_limit(1, INSTALL_TIMEOUT - s(10)), s(20));
    assert_eq!(try_limit(2, INSTALL_TIMEOUT - s(30)), INSTALL_TIMEOUT - s(30), "the third try takes what's left");
    assert_eq!(try_limit(5, s(7)), s(7));
    assert_eq!(try_limit(0, s(4)), s(4), "never past the install's bound");
    assert!(s(10) + s(20) < INSTALL_TIMEOUT, "the first two tries leave the third room");
}
