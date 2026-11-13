//! SME-516: a refresh of an applied top-level persona (R1–R9). The seat's
//! pod is a fake checkout; the refresh reads a fixture remote on the test's
//! own disk, recorded as the conversation's repo (`file://`, which only test
//! builds fetch): review 1 H1 moved the read off the pod.

use super::repo::{FakeCheckout, root_with_checkout};
use super::top_level::{persona_md, set};
use super::*;
use crate::api::personas::SetPersona;
use crate::git::remote_read_tests::{Fixture, fixture_of};

const DRIVER: &str = ".agents/personas/driver.md";

/// The newer file: a ceiling, a deny and `git-ssh`.
fn newer() -> String {
    persona_md("driver", "smelt:\n  compact_at: 120000\n  credentials: [git-ssh]\n  tools:\n    deny: [write_file]\n")
}

/// The recorded remote: `main` holds the first set's file (`old`), then
/// `extra`'s, then `newer()` at the tip (`new`); `side` is a pushed branch
/// off `old` that isn't on it.
pub(crate) struct Remote {
    pub fixture: Fixture,
    pub key: String,
    pub old: String,
    pub new: String,
    pub extra: Vec<String>,
    pub side: String,
}

/// A conversation set to `app:driver` (no settings), its repo recorded as
/// a fixture remote holding `extra` between the first set's commit and the
/// tip, and trusted.
async fn applied(pool: &PgPool, extra: &[String]) -> (i64, Arc<FakeCheckout>, Remote) {
    db::test_support::start_ids_clear_of_other_runs(pool).await.expect("ids");
    let first = persona_md("driver", "");
    let mut main: Vec<&str> = vec![first.as_str()];
    main.extend(extra.iter().map(String::as_str));
    let tip = newer();
    main.push(&tip);
    let widened = persona_md("driver", "smelt:\n  credentials: [git-ssh, prod-token]\n");
    let fixture = fixture_of("driver.md", &main, &widened, false);
    let key = crate::git::remote_key(&fixture.url).expect("a key");
    let fake = Arc::new(FakeCheckout {
        parent_personas: vec![DRIVER.to_string(), ".agents/personas/deployer.md".to_string()],
        files: Mutex::new([(DRIVER.to_string(), first.clone()), (".agents/personas/deployer.md".to_string(), persona_md("deployer", ""))].into()),
        ..Default::default()
    });
    let id = root_with_checkout(pool, None, fake.clone()).await;
    sqlx::query("UPDATE conversation_repos SET url = $2, remote_key = $3 WHERE conversation_id = $1")
        .bind(id)
        .bind(&fixture.url)
        .bind(&key)
        .execute(pool)
        .await
        .expect("record the fixture remote");
    db::set_repo_trust(pool, &key, true).await.expect("trust");
    assert!(set(pool, id, "app:driver").await.expect("the first set").applied);
    let n = fixture.main.len();
    let remote = Remote {
        key,
        old: fixture.main[0].clone(),
        new: fixture.main[n - 1].clone(),
        extra: fixture.main[1..n - 1].to_vec(),
        side: fixture.side.clone(),
        fixture,
    };
    (id, fake, remote)
}

/// For `turn::tests::compact_at`'s R10: a conversation set to `app:driver`
/// with no ceiling, and its remote, whose tip (`new`) gives it 120,000.
pub(crate) async fn applied_without_a_ceiling(pool: &PgPool) -> (i64, Remote) {
    let (id, _, remote) = applied(pool, &[]).await;
    (id, remote)
}

async fn refresh(pool: &PgPool, id: i64, persona: &str, commit: Option<&str>) -> Result<SetPersona, String> {
    crate::agents::top_level::set_persona(pool, id, persona, commit).await
}

type Snapshot = (Option<String>, String, serde_json::Value, Option<serde_json::Value>, Vec<String>, chrono::NaiveDateTime);

/// The row's snapshot columns and `applied_at`.
async fn snapshot(pool: &PgPool, id: i64) -> Snapshot {
    sqlx::query_as(
        "SELECT persona_commit, persona_body, persona_settings, rule_chain, credentials, applied_at
         FROM top_level_personas WHERE conversation_id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("the row")
}

/// Holds `id`'s turn lock, as a running turn does, until the sender fires.
async fn hold_lock(id: i64) -> (tokio::task::JoinHandle<()>, tokio::sync::oneshot::Sender<()>) {
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let (is_held, held) = tokio::sync::oneshot::channel::<()>();
    let holder = tokio::spawn(async move {
        crate::turn::between_turns(id, true, || async move {
            let _ = is_held.send(());
            let _ = released.await;
        })
        .await;
    });
    held.await.expect("the lock is held");
    (holder, release)
}

/// R1: refreshed at a newer commit, the row takes its body, settings,
/// chain, Rule G bit and commit; `applied_at` stays.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refresh_at_a_newer_commit_replaces_the_snapshot(pool: PgPool) {
    let (id, _fake, r) = applied(&pool, &[]).await;
    let before = snapshot(&pool, id).await;
    let answer = refresh(&pool, id, "app:driver", Some(&r.new)).await.expect("the refresh");
    assert_eq!(answer.refresh.as_ref().map(|r| r.state.as_str()), Some("applied"), "{answer:?}");
    assert_eq!(answer.commit.as_deref(), Some(r.new.as_str()));
    let (commit, body, settings, chain, credentials, applied_at) = snapshot(&pool, id).await;
    assert_eq!(commit.as_deref(), Some(r.new.as_str()));
    assert!(body.contains("Work as driver."), "{body}");
    assert_eq!(settings["compact_at"], 120000);
    assert_eq!(chain, Some(serde_json::json!([{ "class": "all", "allow": [], "deny": ["write_file"] }])));
    assert_eq!(credentials, vec!["git-ssh".to_string()]);
    assert_eq!(applied_at, before.5, "applied_at is the first set's");
    assert_eq!(db::persona_compact_at(&pool, id).await.expect("ceiling"), Some(120000));
    // No commit means the default branch's tip; an abbreviated id is its commit.
    refresh(&pool, id, "app:driver", Some(&r.old[..10])).await.expect("a refresh at the first commit");
    assert_eq!(snapshot(&pool, id).await.0.as_deref(), Some(r.old.as_str()));
    refresh(&pool, id, "app:driver", None).await.expect("a refresh at the tip");
    assert_eq!(snapshot(&pool, id).await.0.as_deref(), Some(r.new.as_str()));
}

/// Review 1 H1: a seat that forges its pod can't widen its snapshot. Its
/// checkout answers a widened file to every read (as an edited tree or a
/// wrapper `git` would), and its origin is repointed at a repo whose `main`
/// widens it: the refresh asks the pod nothing, reads only the recorded
/// remote, refuses the forged repo's commit, and refuses a recorded URL
/// that isn't the trusted remote.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_forged_pod_cant_widen_the_refresh(pool: PgPool) {
    let (id, fake, r) = applied(&pool, &[]).await;
    let widened = persona_md("driver", "smelt:\n  credentials: [git-ssh, prod-token]\n  tools:\n    allow: [\"*\"]\n");
    fake.files.lock().expect("files").insert(DRIVER.to_string(), widened.clone());
    fake.parent_files.lock().expect("parent files").insert(DRIVER.to_string(), widened.clone());
    let evil = fixture_of("driver.md", &[&widened], &widened, false);

    refresh(&pool, id, "app:driver", None).await.expect("the refresh");
    let (commit, _, settings, chain, credentials, _) = snapshot(&pool, id).await;
    assert_eq!(commit.as_deref(), Some(r.new.as_str()), "the recorded remote's tip");
    assert_eq!(settings["compact_at"], 120000, "the recorded remote's file, not the pod's");
    assert_eq!(settings["credentials"], serde_json::json!(["git-ssh"]), "no prod-token");
    assert_eq!(chain, Some(serde_json::json!([{ "class": "all", "allow": [], "deny": ["write_file"] }])));
    assert_eq!(credentials, vec!["git-ssh".to_string()]);
    let before = snapshot(&pool, id).await;

    let refused = refresh(&pool, id, "app:driver", Some(&evil.main[0])).await.expect_err("the forged repo's commit");
    assert!(refused.starts_with(&format!("{} isn't on main at {}", evil.main[0], r.fixture.url)), "{refused}");
    sqlx::query("UPDATE conversation_repos SET url = $2 WHERE conversation_id = $1")
        .bind(id)
        .bind(&evil.url)
        .execute(&pool)
        .await
        .expect("record the forged remote");
    let refused = refresh(&pool, id, "app:driver", None).await.expect_err("another remote");
    assert!(refused.contains(&format!("isn't {}, the remote whose trust was read", r.key)), "{refused}");
    assert_eq!(snapshot(&pool, id).await, before);
}

/// R2: a refresh naming another persona is refused, and nothing changes.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refresh_keeps_its_persona(pool: PgPool) {
    let (id, _fake, r) = applied(&pool, &[]).await;
    let before = snapshot(&pool, id).await;
    let refused = refresh(&pool, id, "app:deployer", Some(&r.new)).await.expect_err("another persona");
    assert_eq!(refused, "a refresh keeps its persona: this conversation is `app:driver`; start a new conversation for another");
    assert_eq!(snapshot(&pool, id).await, before);
}

/// R3: a commit not on the default branch is refused with that reason,
/// nothing is written, and the GET says so; so is a ref name (review 1
/// L1); a first set takes no commit.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refresh_off_the_default_branch_is_refused(pool: PgPool) {
    let (id, _fake, r) = applied(&pool, &[]).await;
    let before = snapshot(&pool, id).await;
    let refused = refresh(&pool, id, "app:driver", Some(&r.side)).await.expect_err("a side branch");
    assert_eq!(
        refused,
        format!("{} isn't on main at {}: a refresh reads only commits on the remote's default branch", r.side, r.fixture.url)
    );
    assert_eq!(snapshot(&pool, id).await, before);
    let got = crate::agents::top_level::get_persona(&pool, id).await.expect("the GET");
    let state = got.set.and_then(|s| s.refresh).expect("a refresh state");
    assert_eq!((state.state.as_str(), state.why.as_deref()), ("refused", Some(refused.as_str())));

    let refused = refresh(&pool, id, "app:driver", Some("main")).await.expect_err("a ref name");
    assert!(refused.starts_with("\"main\" isn't a commit id"), "{refused}");
    assert_eq!(snapshot(&pool, id).await, before);

    let fresh = root_with_checkout(&pool, Some(true), Arc::new(FakeCheckout::default())).await;
    let refused = refresh(&pool, fresh, "app:driver", Some(&r.new)).await.expect_err("a first set with a commit");
    assert_eq!(refused, "a first set reads the checkout as it is; commit is for a refresh");
}

/// R4: an untrusted remote is refused; so is trust forgotten between the
/// read and the apply, which reads it again under the trust lock before
/// it touches the pod (review 1 M1). The row and the pod are unchanged.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refresh_reads_the_trust_again_in_its_transaction(pool: PgPool) {
    let (id, fake, r) = applied(&pool, &[]).await;
    let pod = db::create_sandbox_pod(&pool, id).await.expect("a live pod row");
    let writes = Arc::new(Mutex::new(0));
    let seen = writes.clone();
    crate::credentials::test_writes::register(
        pod.id,
        Arc::new(move |_| {
            *seen.lock().expect("writes") += 1;
            Box::pin(async { Ok(()) })
        }),
    );
    let before = snapshot(&pool, id).await;
    let (reached, release) = test_hooks::pause(test_hooks::TOP_LEVEL_AFTER_READ, id);
    let refreshing = tokio::spawn({
        let (pool, new) = (pool.clone(), r.new.clone());
        async move { refresh(&pool, id, "app:driver", Some(&new)).await }
    });
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the refresh reads").expect("paused");
    db::set_repo_trust(&pool, &r.key, false).await.expect("forget the trust");
    release.send(()).expect("release");
    let refused = refreshing.await.expect("joined").expect_err("refused");
    assert!(refused.contains("doesn't trust this repo's remote"), "{refused}");
    assert_eq!(snapshot(&pool, id).await, before);
    assert_eq!(*writes.lock().expect("writes"), 0, "nothing written into the pod");
    assert!(fake.restarts.lock().expect("restarts").is_empty());

    let refused = refresh(&pool, id, "app:driver", Some(&r.new)).await.expect_err("untrusted at the read");
    assert!(refused.contains("doesn't trust"), "{refused}");
    assert_eq!(snapshot(&pool, id).await, before);
}

/// R5 and R6: with the turn lock held the refresh is `pending` and writes
/// nothing; a second is refused meanwhile. Released, it applies before a
/// turn queued after the read, which reads the new ceiling. Then a third
/// is allowed.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refresh_waits_for_the_turn_and_one_waits_at_a_time(pool: PgPool) {
    let (id, _fake, r) = applied(&pool, &[]).await;
    let before = snapshot(&pool, id).await;
    let (holder, release) = hold_lock(id).await;
    let answer = refresh(&pool, id, "app:driver", Some(&r.new)).await.expect("pending");
    let state = answer.refresh.expect("a refresh state");
    assert_eq!((state.state.as_str(), state.commit.as_deref()), ("pending", Some(r.new.as_str())));
    assert_eq!(snapshot(&pool, id).await, before, "nothing written while the turn runs");

    let refused = refresh(&pool, id, "app:driver", Some(&r.new)).await.expect_err("a second while one waits");
    assert_eq!(refused, format!("a refresh at {} is already waiting for this conversation's turn to end", &r.new[..7]));

    let next_turn = tokio::spawn({
        let pool = pool.clone();
        async move { crate::turn::between_turns(id, true, || db::persona_compact_at(&pool, id)).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    release.send(()).expect("end the turn");
    holder.await.expect("the holder");
    let ceiling = next_turn.await.expect("joined").expect("ran").expect("read");
    assert_eq!(ceiling, Some(120000), "the turn queued after the read runs on the new snapshot");
    assert_eq!(snapshot(&pool, id).await.0.as_deref(), Some(r.new.as_str()));
    let got = crate::agents::top_level::get_persona(&pool, id).await.expect("the GET");
    assert_eq!(got.set.and_then(|s| s.refresh).map(|r| r.state), Some("applied".to_string()));

    let again = refresh(&pool, id, "app:driver", Some(&r.old)).await.expect("a third, once the first applied");
    assert_eq!(again.refresh.map(|r| r.state), Some("applied".to_string()));
}

/// R7: new resources with a command running on a live pod are refused with
/// the count, and nothing changes; with none running, the pod restarts
/// and the row is written.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refresh_with_new_resources_is_refused_while_commands_run(pool: PgPool) {
    let sized = persona_md("driver", "smelt:\n  resources:\n    docker: { memory_request: 1Gi, memory_limit: 2Gi }\n");
    let (id, fake, r) = applied(&pool, &[sized]).await;
    let sized = r.extra[0].clone();
    super::restart_owed::command(&pool, id, "toolu_running", false).await;
    // The live pod's grant writes land in a capture, not a sandbox.
    let pod = db::list_sandbox_pods(&pool, id).await.expect("pods").pop().expect("a live pod");
    crate::credentials::test_writes::register(pod.id, Arc::new(|_| Box::pin(async { Ok(()) })));
    let before = snapshot(&pool, id).await;
    let refused = refresh(&pool, id, "app:driver", Some(&sized)).await.expect_err("a command runs");
    assert_eq!(
        refused,
        format!(
            "the persona at {} asks for other pod resources, and restarting the pod would end its 1 running \
             commands: stop them, then refresh again",
            &sized[..7]
        )
    );
    assert_eq!(db::conversation_pod_resources(&pool, id).await.expect("read"), None);
    assert!(fake.restarts.lock().expect("restarts").is_empty());
    assert_eq!(snapshot(&pool, id).await, before);

    db::mark_terminal_command_finished(&pool, "toolu_running", 0).await.expect("finish it");
    refresh(&pool, id, "app:driver", Some(&sized)).await.expect("no command runs");
    assert_eq!(*fake.restarts.lock().expect("restarts"), vec![id]);
    let resources = db::conversation_pod_resources(&pool, id).await.expect("read").expect("set");
    assert_eq!(resources["docker"]["memory_limit"], "2Gi");
    assert_eq!(snapshot(&pool, id).await.0.as_deref(), Some(sized.as_str()));
}

/// R8: the grant. The pod's files are written for the new grant before
/// the row changes (here dropping `git-ssh`'s key); a failed transaction
/// leaves the row old and puts the pod back to its grant, and a put-back
/// that keeps failing stops the pod (review 1 M1).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refresh_writes_the_pod_before_the_row(pool: PgPool) {
    let (id, fake, r) = applied(&pool, &[persona_md("driver", "")]).await;
    let keyless = r.extra[0].clone();
    db::create_ssh_key(&pool, "id_ed25519", "pub", "PRIVATE-MARKER").await.expect("a key");
    refresh(&pool, id, "app:driver", Some(&r.new)).await.expect("to the key's grant");
    let pod = db::create_sandbox_pod(&pool, id).await.expect("a live pod row");
    // Each write: whether it holds the key, and the row's commit then.
    let writes = Arc::new(Mutex::new(Vec::<(bool, Option<String>)>::new()));
    let fail_puts = Arc::new(Mutex::new(false));
    let (seen, probe, failing) = (writes.clone(), pool.clone(), fail_puts.clone());
    crate::credentials::test_writes::register(
        pod.id,
        Arc::new(move |files: Vec<crate::credentials::PodFile>| {
            let (seen, probe, failing) = (seen.clone(), probe.clone(), failing.clone());
            Box::pin(async move {
                let commit = db::get_top_level_persona(&probe, id).await.expect("row").and_then(|r| r.persona_commit);
                let key = files.iter().any(|f| f.path.ends_with("/keys/id_ed25519"));
                seen.lock().expect("writes").push((key, commit));
                let fail = *failing.lock().expect("failing") && key;
                if fail { Err("a test failed the put-back".to_string()) } else { Ok(()) }
            })
        }),
    );

    refresh(&pool, id, "app:driver", Some(&keyless)).await.expect("dropping git-ssh");
    assert_eq!(*writes.lock().expect("writes"), vec![(false, Some(r.new.clone()))], "keyless, before the row");
    assert_eq!(snapshot(&pool, id).await.4, Vec::<String>::new());

    writes.lock().expect("writes").clear();
    test_hooks::fail_next(test_hooks::TOP_LEVEL_REFRESH_TX, id);
    let refused = refresh(&pool, id, "app:driver", Some(&r.new)).await.expect_err("the transaction fails");
    assert_eq!(refused, "a test failed the refresh's transaction");
    assert_eq!(snapshot(&pool, id).await.0.as_deref(), Some(keyless.as_str()), "the row keeps the old snapshot");
    let old = Some(keyless.clone());
    assert_eq!(*writes.lock().expect("writes"), vec![(true, old.clone()), (false, old)], "new grant, then put back");
    assert!(fake.stops.lock().expect("stops").is_empty());

    // The key's grant goes back to `refresh`'s row: a failing put-back of a
    // keyed grant is retried, then the pod is stopped.
    refresh(&pool, id, "app:driver", Some(&r.new)).await.expect("back to the key's grant");
    *fail_puts.lock().expect("failing") = true;
    writes.lock().expect("writes").clear();
    test_hooks::fail_next(test_hooks::TOP_LEVEL_REFRESH_TX, id);
    let refused = refresh(&pool, id, "app:driver", Some(&keyless)).await.expect_err("the transaction fails");
    assert_eq!(refused, "a test failed the refresh's transaction; its pod couldn't be put back to its grant, so it was stopped");
    let put_backs = writes.lock().expect("writes").iter().filter(|(key, _)| *key).count();
    assert_eq!(put_backs, 3, "each put-back attempt wrote the row's keyed grant");
    assert_eq!(*fake.stops.lock().expect("stops"), vec![id]);
}

/// Review 1, seam B L1: a refresh whose conversation is being deleted is
/// refused before it restarts or writes the pod.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refresh_of_a_conversation_being_deleted_touches_no_pod(pool: PgPool) {
    let sized = persona_md("driver", "smelt:\n  resources:\n    docker: { memory_request: 1Gi, memory_limit: 2Gi }\n");
    let (id, fake, r) = applied(&pool, &[sized]).await;
    let pod = db::create_sandbox_pod(&pool, id).await.expect("a live pod row");
    let writes = Arc::new(Mutex::new(0));
    let seen = writes.clone();
    crate::credentials::test_writes::register(
        pod.id,
        Arc::new(move |_| {
            *seen.lock().expect("writes") += 1;
            Box::pin(async { Ok(()) })
        }),
    );
    sqlx::query("UPDATE conversations SET deleting_at = now() WHERE id = $1").bind(id).execute(&pool).await.expect("deleting");
    let refused = refresh(&pool, id, "app:driver", Some(&r.extra[0])).await.expect_err("being deleted");
    assert_eq!(refused, "this conversation is being deleted");
    assert!(fake.restarts.lock().expect("restarts").is_empty(), "no restart");
    assert_eq!(db::conversation_pod_resources(&pool, id).await.expect("read"), None);
    assert_eq!(*writes.lock().expect("writes"), 0, "no pod write");
}

/// Review 1, seam B L2 and L3: a refresh whose claim is dropped while
/// still pending (its task panicked) records `refused`, so the next isn't
/// refused as "already waiting"; and a forgotten conversation's late
/// refresh doesn't bring its entry back.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_dropped_refresh_is_refused_and_a_forgotten_one_stays_forgotten(pool: PgPool) {
    let (id, _fake, r) = applied(&pool, &[]).await;
    test_hooks::fail_next(test_hooks::TOP_LEVEL_REFRESH_PANIC, id);
    refresh(&pool, id, "app:driver", Some(&r.new)).await.expect_err("the refresh's task panicked");
    let state = crate::agents::top_level_refresh::refresh_of(id).expect("an entry");
    assert_eq!((state.state.as_str(), state.why.as_deref()), ("refused", Some(crate::agents::top_level_refresh::ENDED_UNAPPLIED)));
    refresh(&pool, id, "app:driver", Some(&r.new)).await.expect("the next refresh isn't refused");

    let (reached, release) = test_hooks::pause(test_hooks::TOP_LEVEL_AFTER_READ, id);
    let refreshing = tokio::spawn({
        let (pool, old) = (pool.clone(), r.old.clone());
        async move { refresh(&pool, id, "app:driver", Some(&old)).await }
    });
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the refresh reads").expect("paused");
    crate::agents::top_level_refresh::forget(id);
    release.send(()).expect("release");
    let _ = refreshing.await.expect("joined");
    assert_eq!(crate::agents::top_level_refresh::refresh_of(id), None, "the forgotten entry stays forgotten");
}

/// R9: the GET shows the snapshot's settings and the refresh, and says a
/// started conversation's persona can only be refreshed.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_get_shows_the_settings_and_the_refresh(pool: PgPool) {
    let (id, _fake, r) = applied(&pool, &[]).await;
    let got = crate::agents::top_level::get_persona(&pool, id).await.expect("the GET");
    assert_eq!(got.can_set.as_deref(), Some(crate::agents::top_level::REFRESH_ONLY_TEXT));
    let set_to = got.set.expect("set");
    assert!(set_to.settings["compact_at"].is_null(), "{:?}", set_to.settings);
    assert_eq!(set_to.refresh, None);
    refresh(&pool, id, "app:driver", None).await.expect("the refresh");
    let set_to = crate::agents::top_level::get_persona(&pool, id).await.expect("the GET").set.expect("set");
    assert_eq!(set_to.settings["compact_at"], 120000);
    let state = set_to.refresh.expect("the refresh");
    assert_eq!((state.state.as_str(), state.commit.as_deref(), state.why), ("applied", Some(r.new.as_str()), None));
}
