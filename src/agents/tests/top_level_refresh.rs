//! SME-516: a refresh of an applied top-level persona (R1–R9), against a
//! fake checkout that serves the persona file at commits on its default
//! branch (`FakeCheckout::commit_files`).

use super::repo::{FakeCheckout, root_with_checkout};
use super::top_level::{KEY, persona_md, set};
use super::*;
use crate::api::personas::SetPersona;

const DRIVER: &str = ".agents/personas/driver.md";
/// The default branch's commits: the first set's, and a newer one.
const OLD: &str = "a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0";
const NEW: &str = "b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1";
/// A pushed branch's commit, not on the default branch.
const SIDE: &str = "c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2";

/// The newer file: a ceiling, a deny and `git-ssh`.
fn newer() -> String {
    persona_md("driver", "smelt:\n  compact_at: 120000\n  credentials: [git-ssh]\n  tools:\n    deny: [write_file]\n")
}

/// A conversation set to `app:driver` (no settings), with the fake that
/// serves `OLD` (as the set read it), `NEW` (`newer()`, the tip) and
/// `extra`, and refuses `SIDE`.
async fn applied(pool: &PgPool, extra: &[(&str, String)]) -> (i64, Arc<FakeCheckout>) {
    db::test_support::start_ids_clear_of_other_runs(pool).await.expect("ids");
    let first = persona_md("driver", "");
    let mut at: std::collections::HashMap<(String, String), String> =
        [((OLD.to_string(), DRIVER.to_string()), first.clone()), ((NEW.to_string(), DRIVER.to_string()), newer())].into();
    at.extend(extra.iter().map(|(sha, file)| ((sha.to_string(), DRIVER.to_string()), file.clone())));
    let fake = Arc::new(FakeCheckout {
        parent_personas: vec![DRIVER.to_string(), ".agents/personas/deployer.md".to_string()],
        files: Mutex::new([(DRIVER.to_string(), first), (".agents/personas/deployer.md".to_string(), persona_md("deployer", ""))].into()),
        commit_files: Mutex::new(at),
        default_tip: Mutex::new(Some(NEW.to_string())),
        side_commits: vec![SIDE.to_string()],
        ..Default::default()
    });
    let id = root_with_checkout(pool, Some(true), fake.clone()).await;
    assert!(set(pool, id, "app:driver").await.expect("the first set").applied);
    (id, fake)
}

/// For `turn::tests::compact_at`'s R10: a conversation set to `app:driver`
/// with no ceiling, which a refresh at `NEW` (returned) gives 120,000.
pub(crate) async fn applied_without_a_ceiling(pool: &PgPool) -> (i64, &'static str) {
    (applied(pool, &[]).await.0, NEW)
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
    let (id, _fake) = applied(&pool, &[]).await;
    let before = snapshot(&pool, id).await;
    let answer = refresh(&pool, id, "app:driver", Some(NEW)).await.expect("the refresh");
    assert_eq!(answer.refresh.as_ref().map(|r| r.state.as_str()), Some("applied"), "{answer:?}");
    assert_eq!(answer.commit.as_deref(), Some(NEW));
    let (commit, body, settings, chain, credentials, applied_at) = snapshot(&pool, id).await;
    assert_eq!(commit.as_deref(), Some(NEW));
    assert!(body.contains("Work as driver."), "{body}");
    assert_eq!(settings["compact_at"], 120000);
    assert_eq!(chain, Some(serde_json::json!([{ "class": "all", "allow": [], "deny": ["write_file"] }])));
    assert_eq!(credentials, vec!["git-ssh".to_string()]);
    assert_eq!(applied_at, before.5, "applied_at is the first set's");
    assert_eq!(db::persona_compact_at(&pool, id).await.expect("ceiling"), Some(120000));
    // No commit means the default branch's tip.
    refresh(&pool, id, "app:driver", None).await.expect("a refresh at the tip");
    assert_eq!(snapshot(&pool, id).await.0.as_deref(), Some(NEW));
}

/// R2: a refresh naming another persona is refused, and nothing changes.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refresh_keeps_its_persona(pool: PgPool) {
    let (id, _fake) = applied(&pool, &[]).await;
    let before = snapshot(&pool, id).await;
    let refused = refresh(&pool, id, "app:deployer", Some(NEW)).await.expect_err("another persona");
    assert_eq!(refused, "a refresh keeps its persona: this conversation is `app:driver`; start a new conversation for another");
    assert_eq!(snapshot(&pool, id).await, before);
}

/// R3: a commit not on the default branch is refused with that reason,
/// nothing is written, and the GET says so; a first set takes no commit.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refresh_off_the_default_branch_is_refused(pool: PgPool) {
    let (id, _fake) = applied(&pool, &[]).await;
    let before = snapshot(&pool, id).await;
    let refused = refresh(&pool, id, "app:driver", Some(SIDE)).await.expect_err("a side branch");
    assert!(refused.contains("isn't on origin/main"), "{refused}");
    assert_eq!(snapshot(&pool, id).await, before);
    let got = crate::agents::top_level::get_persona(&pool, id).await.expect("the GET");
    let state = got.set.and_then(|s| s.refresh).expect("a refresh state");
    assert_eq!((state.state.as_str(), state.why.as_deref()), ("refused", Some(refused.as_str())));

    let fresh = root_with_checkout(&pool, Some(true), Arc::new(FakeCheckout::default())).await;
    let refused = refresh(&pool, fresh, "app:driver", Some(NEW)).await.expect_err("a first set with a commit");
    assert_eq!(refused, "a first set reads the checkout as it is; commit is for a refresh");
}

/// R4: an untrusted remote is refused; so is trust forgotten between the
/// read and the transaction, which reads it again. The row is unchanged.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refresh_reads_the_trust_again_in_its_transaction(pool: PgPool) {
    let (id, _fake) = applied(&pool, &[]).await;
    let before = snapshot(&pool, id).await;
    let (reached, release) = test_hooks::pause(test_hooks::TOP_LEVEL_AFTER_READ, id);
    let refreshing = tokio::spawn({
        let pool = pool.clone();
        async move { refresh(&pool, id, "app:driver", Some(NEW)).await }
    });
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the refresh reads").expect("paused");
    db::set_repo_trust(&pool, KEY, false).await.expect("forget the trust");
    release.send(()).expect("release");
    let refused = refreshing.await.expect("joined").expect_err("refused");
    assert!(refused.contains("doesn't trust this repo's remote"), "{refused}");
    assert_eq!(snapshot(&pool, id).await, before);

    let refused = refresh(&pool, id, "app:driver", Some(NEW)).await.expect_err("untrusted at the read");
    assert!(refused.contains("doesn't trust"), "{refused}");
    assert_eq!(snapshot(&pool, id).await, before);
}

/// R5 and R6: with the turn lock held the refresh is `pending` and writes
/// nothing; a second is refused meanwhile. Released, it applies before a
/// turn queued after the request, which reads the new ceiling. Then a
/// third is allowed.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refresh_waits_for_the_turn_and_one_waits_at_a_time(pool: PgPool) {
    let (id, _fake) = applied(&pool, &[]).await;
    let before = snapshot(&pool, id).await;
    let (holder, release) = hold_lock(id).await;
    let answer = refresh(&pool, id, "app:driver", Some(NEW)).await.expect("pending");
    let state = answer.refresh.expect("a refresh state");
    assert_eq!((state.state.as_str(), state.commit.as_deref()), ("pending", Some(NEW)));
    assert_eq!(snapshot(&pool, id).await, before, "nothing written while the turn runs");

    let refused = refresh(&pool, id, "app:driver", Some(NEW)).await.expect_err("a second while one waits");
    assert_eq!(refused, "a refresh at b1b1b1b is already waiting for this conversation's turn to end");

    let next_turn = tokio::spawn({
        let pool = pool.clone();
        async move { crate::turn::between_turns(id, true, || db::persona_compact_at(&pool, id)).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    release.send(()).expect("end the turn");
    holder.await.expect("the holder");
    let ceiling = next_turn.await.expect("joined").expect("ran").expect("read");
    assert_eq!(ceiling, Some(120000), "the turn queued after the request runs on the new snapshot");
    assert_eq!(snapshot(&pool, id).await.0.as_deref(), Some(NEW));
    let got = crate::agents::top_level::get_persona(&pool, id).await.expect("the GET");
    assert_eq!(got.set.and_then(|s| s.refresh).map(|r| r.state), Some("applied".to_string()));

    let again = refresh(&pool, id, "app:driver", Some(OLD)).await.expect("a third, once the first applied");
    assert_eq!(again.refresh.map(|r| r.state), Some("applied".to_string()));
}

/// R7: new resources with a command running on a live pod are refused with
/// the count, and nothing changes; with none running, the pod restarts
/// and the row is written.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refresh_with_new_resources_is_refused_while_commands_run(pool: PgPool) {
    const SIZED: &str = "d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3";
    let sized = persona_md("driver", "smelt:\n  resources:\n    docker: { memory_request: 1Gi, memory_limit: 2Gi }\n");
    let (id, fake) = applied(&pool, &[(SIZED, sized)]).await;
    super::restart_owed::command(&pool, id, "toolu_running", false).await;
    // The live pod's grant writes land in a capture, not a sandbox.
    let pod = db::list_sandbox_pods(&pool, id).await.expect("pods").pop().expect("a live pod");
    crate::credentials::test_writes::register(pod.id, Arc::new(|_| Box::pin(async { Ok(()) })));
    let before = snapshot(&pool, id).await;
    let refused = refresh(&pool, id, "app:driver", Some(SIZED)).await.expect_err("a command runs");
    assert_eq!(
        refused,
        "the persona at d3d3d3d asks for other pod resources, and restarting the pod would end its 1 running \
         commands: stop them, then refresh again"
    );
    assert_eq!(db::conversation_pod_resources(&pool, id).await.expect("read"), None);
    assert!(fake.restarts.lock().expect("restarts").is_empty());
    assert_eq!(snapshot(&pool, id).await, before);

    db::mark_terminal_command_finished(&pool, "toolu_running", 0).await.expect("finish it");
    refresh(&pool, id, "app:driver", Some(SIZED)).await.expect("no command runs");
    assert_eq!(*fake.restarts.lock().expect("restarts"), vec![id]);
    let resources = db::conversation_pod_resources(&pool, id).await.expect("read").expect("set");
    assert_eq!(resources["docker"]["memory_limit"], "2Gi");
    assert_eq!(snapshot(&pool, id).await.0.as_deref(), Some(SIZED));
}

/// R8: the grant. The pod's files are written for the new grant before
/// the row changes (here dropping `git-ssh`'s key); a failed transaction
/// leaves the row old and puts the pod back to its grant.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refresh_writes_the_pod_before_the_row(pool: PgPool) {
    const KEYLESS: &str = "e4e4e4e4e4e4e4e4e4e4e4e4e4e4e4e4e4e4e4e4";
    let (id, _fake) = applied(&pool, &[(KEYLESS, persona_md("driver", ""))]).await;
    db::create_ssh_key(&pool, "id_ed25519", "pub", "PRIVATE-MARKER").await.expect("a key");
    refresh(&pool, id, "app:driver", Some(NEW)).await.expect("to the key's grant");
    let pod = db::create_sandbox_pod(&pool, id).await.expect("a live pod row");
    // Each write: whether it holds the key, and the row's commit then.
    let writes = Arc::new(Mutex::new(Vec::<(bool, Option<String>)>::new()));
    let (seen, probe) = (writes.clone(), pool.clone());
    crate::credentials::test_writes::register(
        pod.id,
        Arc::new(move |files: Vec<crate::credentials::PodFile>| {
            let (seen, probe) = (seen.clone(), probe.clone());
            Box::pin(async move {
                let commit = db::get_top_level_persona(&probe, id).await.expect("row").and_then(|r| r.persona_commit);
                let key = files.iter().any(|f| f.path.ends_with("/keys/id_ed25519"));
                seen.lock().expect("writes").push((key, commit));
                Ok(())
            })
        }),
    );

    refresh(&pool, id, "app:driver", Some(KEYLESS)).await.expect("dropping git-ssh");
    assert_eq!(*writes.lock().expect("writes"), vec![(false, Some(NEW.to_string()))], "keyless, before the row");
    assert_eq!(snapshot(&pool, id).await.4, Vec::<String>::new());

    writes.lock().expect("writes").clear();
    test_hooks::fail_next(test_hooks::TOP_LEVEL_REFRESH_TX, id);
    let refused = refresh(&pool, id, "app:driver", Some(NEW)).await.expect_err("the transaction fails");
    assert_eq!(refused, "a test failed the refresh's transaction");
    assert_eq!(snapshot(&pool, id).await.0.as_deref(), Some(KEYLESS), "the row keeps the old snapshot");
    let keyless = Some(KEYLESS.to_string());
    assert_eq!(*writes.lock().expect("writes"), vec![(true, keyless.clone()), (false, keyless)], "new grant, then put back");
}

/// R9: the GET shows the snapshot's settings and the refresh, and says a
/// started conversation's persona can only be refreshed.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_get_shows_the_settings_and_the_refresh(pool: PgPool) {
    let (id, _fake) = applied(&pool, &[]).await;
    let got = crate::agents::top_level::get_persona(&pool, id).await.expect("the GET");
    assert_eq!(got.can_set.as_deref(), Some(crate::agents::top_level::REFRESH_ONLY_TEXT));
    let set_to = got.set.expect("set");
    assert!(set_to.settings["compact_at"].is_null(), "{:?}", set_to.settings);
    assert_eq!(set_to.refresh, None);
    refresh(&pool, id, "app:driver", None).await.expect("the refresh");
    let set_to = crate::agents::top_level::get_persona(&pool, id).await.expect("the GET").set.expect("set");
    assert_eq!(set_to.settings["compact_at"], 120000);
    let state = set_to.refresh.expect("the refresh");
    assert_eq!((state.state.as_str(), state.commit.as_deref(), state.why), ("applied", Some(NEW), None));
}
