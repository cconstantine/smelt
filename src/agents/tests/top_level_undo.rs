//! SME-481 review 1: a set that fails or is cut off puts everything back
//! before any persona-less turn can run (M1, M2), the sweep waits for a set
//! in progress (L3), a model change racing a set isn't overwritten (L1),
//! and the AGENTS.md load at the set: loaded, or its card deleted with the
//! failed set (L2, L5).

use super::repo::{FakeCheckout, root_with_checkout};
use super::top_level::{KEY, OPUS, conversation_with, persona_md, providers, row, set, stored_model};
use super::*;

/// A fake checkout holding `persona` as `.agents/personas/<name>.md`, with
/// `agents_md` as its AGENTS.md when set.
fn fake(name: &str, persona: String, agents_md: Option<&str>) -> Arc<FakeCheckout> {
    let rel = format!(".agents/personas/{name}.md");
    Arc::new(FakeCheckout {
        agents_md: agents_md.map(str::to_string),
        parent_personas: vec![rel.clone()],
        files: Mutex::new([(rel, persona)].into()),
        ..Default::default()
    })
}

/// Records the checkout's AGENTS.md, as a clone that found one does.
async fn with_agents_md(pool: &PgPool, id: i64) {
    sqlx::query("UPDATE conversation_repos SET agents_files = ARRAY['AGENTS.md'] WHERE conversation_id = $1")
        .bind(id)
        .execute(pool)
        .await
        .expect("record the AGENTS.md");
}

/// M1: an apply that fails after the persona moved the conversation's
/// model and pod resources puts both back: the persona-less conversation
/// asks for nothing the persona did.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_failed_set_puts_the_model_and_resources_back(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids");
    let (_, claude_code) = providers(&pool, &["default-model", "claude-opus-5-5"], &["claude-opus-4-x", "claude-opus-5-5"]).await;
    let smelt = format!("{OPUS}  resources:\n    docker: {{ memory_request: 1Gi, memory_limit: 2Gi }}\n");
    let checkout = fake("builder", persona_md("builder", &smelt), None);
    let id = root_with_checkout(&pool, Some(true), checkout.clone()).await;
    db::set_conversation_model(&pool, id, claude_code, "claude-opus-4-x").await.expect("on ClaudeCode");
    test_hooks::fail_next(test_hooks::TOP_LEVEL_APPLY, id);
    set(&pool, id, "app:builder").await.expect_err("the apply fails");
    assert_eq!(row(&pool, id).await, None);
    assert_eq!(stored_model(&pool, id).await, (Some(claude_code), Some("claude-opus-4-x".to_string())), "the model is put back");
    assert_eq!(db::conversation_pod_resources(&pool, id).await.expect("read"), None, "the resources are put back");
    assert_eq!(*checkout.restarts.lock().expect("restarts"), vec![id], "sized once; no pod to restart back");
}

/// M2: a failed apply whose pod can't be put back keeps the row unapplied,
/// so no persona-less turn runs on a pod that may hold the persona's
/// grant; the next restart's sweep puts it back and lets turns in.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_set_whose_pod_cant_be_put_back_keeps_refusing_turns(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let id = conversation_with(&pool, Some(true), &[("driver", persona_md("driver", ""))]).await;
    test_hooks::fail_next(test_hooks::TOP_LEVEL_APPLY, id);
    test_hooks::fail_next(test_hooks::TOP_LEVEL_PUT_BACK, id);
    let refused = set(&pool, id, "app:driver").await.expect_err("the apply fails");
    assert_eq!(row(&pool, id).await, Some(("driver".to_string(), false)), "kept unapplied: {refused}");
    let turn = crate::turn::start_turn(pool.clone(), id, "Hi.".to_string()).await.expect_err("refused");
    assert!(turn.to_string().contains("still being applied"), "{turn}");
    restart(&pool, &[id]).await;
    assert_eq!(row(&pool, id).await, None, "the sweep put it back");
    crate::turn::start_turn(pool.clone(), id, "Hi.".to_string()).await.expect("admitted now");
    eventually("the turn ends", || async { !crate::turn::turn_running(id) }).await;
}

/// L3: the startup sweep runs while smelt already serves, so it waits for
/// a set in progress (its turn lock) rather than deleting its row mid-apply.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_sweep_waits_for_a_set_in_progress(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let id = conversation_with(&pool, Some(true), &[("driver", persona_md("driver", ""))]).await;
    let (reached, release) = test_hooks::pause(test_hooks::TOP_LEVEL_AFTER_INSERT, id);
    let setting = tokio::spawn({
        let pool = pool.clone();
        async move { set(&pool, id, "app:driver").await }
    });
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the set inserts").expect("paused");
    let sweeping = tokio::spawn({
        let pool = pool.clone();
        async move {
            for wake in startup_sweep(&pool).await {
                let _ = wake.await;
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(row(&pool, id).await, Some(("driver".to_string(), false)), "the sweep waits for the set");
    release.send(()).expect("release the set");
    let set_to = tokio::time::timeout(Duration::from_secs(10), setting).await.expect("the set ends").expect("joined");
    assert!(set_to.expect("the set").applied);
    tokio::time::timeout(Duration::from_secs(30), sweeping).await.expect("the sweep ends").expect("joined");
    assert_eq!(row(&pool, id).await, Some(("driver".to_string(), true)));
}

/// L1: a model change landing between the persona's model resolving and
/// the set's transaction is the user's: the set is refused, not pinned
/// over it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_model_change_racing_a_set_isnt_overwritten(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids");
    let (anthropic, claude_code) =
        providers(&pool, &["default-model", "claude-opus-5-5"], &["claude-opus-4-x", "claude-opus-5-5"]).await;
    let id = conversation_with(&pool, Some(true), &[("driver", persona_md("driver", OPUS))]).await;
    db::set_conversation_model(&pool, id, claude_code, "claude-opus-4-x").await.expect("on ClaudeCode");
    let (reached, release) = test_hooks::pause(test_hooks::TOP_LEVEL_AFTER_READ, id);
    let setting = tokio::spawn({
        let pool = pool.clone();
        async move { set(&pool, id, "app:driver").await }
    });
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the set reads").expect("paused");
    db::set_conversation_model(&pool, id, anthropic, "default-model").await.expect("the user's switch");
    release.send(()).expect("release the set");
    let refused = tokio::time::timeout(Duration::from_secs(10), setting).await.expect("ends").expect("joined").expect_err("refused");
    assert!(refused.contains("model changed"), "{refused}");
    assert_eq!(stored_model(&pool, id).await, (Some(anthropic), Some("default-model".to_string())), "the user's choice stands");
    assert_eq!(row(&pool, id).await, None);
}

/// L5: the set loads the checkout's AGENTS.md, as a child's start does.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_set_loads_the_checkouts_agents_md(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids");
    let id = root_with_checkout(&pool, Some(true), fake("driver", persona_md("driver", ""), Some("Run the tests."))).await;
    with_agents_md(&pool, id).await;
    set(&pool, id, "app:driver").await.expect("the set");
    let loaded = db::list_loaded_instructions(&pool, id).await.expect("loaded");
    assert_eq!(loaded.iter().map(|l| (l.path.as_str(), l.content.as_str())).collect::<Vec<_>>(), [("AGENTS.md", "Run the tests.")]);
}

/// L2: trust forgotten before the set's AGENTS.md load: the load files a
/// card and the set fails; the card goes with it, as a child start's does.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_card_a_failed_set_filed_is_deleted(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids");
    let id = root_with_checkout(&pool, Some(true), fake("driver", persona_md("driver", ""), Some("Rules."))).await;
    with_agents_md(&pool, id).await;
    let (reached, release) = test_hooks::pause(test_hooks::BEFORE_AGENTS_MD_LOAD, id);
    let setting = tokio::spawn({
        let pool = pool.clone();
        async move { set(&pool, id, "app:driver").await }
    });
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the set loads").expect("paused");
    db::delete_repo_trust(&pool, KEY).await.expect("forget");
    release.send(()).expect("release the set");
    let refused = tokio::time::timeout(Duration::from_secs(10), setting).await.expect("ends").expect("joined").expect_err("refused");
    assert!(refused.contains("no longer trusts"), "{refused}");
    assert_eq!(row(&pool, id).await, None);
    assert!(db::list_instruction_requests(&pool, id).await.expect("cards").is_empty(), "the card is deleted");
}
