//! SME-481: a top-level conversation set to a persona, once, after its repo
//! is attached and before its first message — "started" (S1–S5), the trust
//! gate (T1–T3) and the model rule (M1–M4), against a fake checkout.

use super::repo::{FakeCheckout, root_with_checkout};
use super::*;
use crate::api::personas::SetPersona;

pub(super) const KEY: &str = "example.com/team/app";

/// A persona file named `name`, its description carrying a marker an
/// untrusted listing must never show, with `smelt` as its settings block.
pub(super) fn persona_md(name: &str, smelt: &str) -> String {
    format!("---\nname: {name}\ndescription: The {name} persona, marker-{name}-inside.\n{smelt}---\nWork as {name}.\n")
}

/// A top-level conversation with a ready checkout `app` of the test remote,
/// trusted as `trust` says, holding `personas` (`(name, file)`).
pub(super) async fn conversation_with(pool: &PgPool, trust: Option<bool>, personas: &[(&str, String)]) -> i64 {
    let rel = |name: &str| format!(".agents/personas/{name}.md");
    let fake = FakeCheckout {
        parent_personas: personas.iter().map(|(name, _)| rel(name)).collect(),
        files: Mutex::new(personas.iter().map(|(name, file)| (rel(name), file.clone())).collect()),
        ..Default::default()
    };
    root_with_checkout(pool, trust, Arc::new(fake)).await
}

pub(super) async fn set(pool: &PgPool, id: i64, persona: &str) -> Result<SetPersona, String> {
    crate::agents::top_level::set_persona(pool, id, persona).await
}

/// The conversation's top-level persona row: its name and whether applied.
pub(super) async fn row(pool: &PgPool, id: i64) -> Option<(String, bool)> {
    db::get_top_level_persona(pool, id).await.expect("read the row").map(|r| (r.persona_name, r.applied))
}

/// S1: a conversation with a message has started: refused, no row.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_persona_is_refused_once_a_message_exists(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids");
    let id = conversation_with(&pool, Some(true), &[("driver", persona_md("driver", ""))]).await;
    db::create_message(&pool, id, "user", &[ContentBlock::Text { text: "hi".to_string() }]).await.expect("a message");
    let refused = set(&pool, id, "app:driver").await.expect_err("refused");
    assert!(refused.contains("has started"), "{refused}");
    assert_eq!(row(&pool, id).await, None);
}

/// S2: no swap — a second set is refused before and after a message, and
/// the first persona stands.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_second_set_is_refused_before_and_after_a_message(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids");
    let personas = [("driver", persona_md("driver", "")), ("deployer", persona_md("deployer", ""))];
    let id = conversation_with(&pool, Some(true), &personas).await;
    let set_to = set(&pool, id, "app:driver").await.expect("the first set");
    assert_eq!((set_to.label.as_str(), set_to.applied), ("app:driver", true));
    let refused = set(&pool, id, "app:deployer").await.expect_err("a swap is refused");
    assert!(refused.contains("has started"), "{refused}");
    assert_eq!(row(&pool, id).await, Some(("driver".to_string(), true)));
    db::create_message(&pool, id, "user", &[ContentBlock::Text { text: "hi".to_string() }]).await.expect("a message");
    let refused = set(&pool, id, "app:deployer").await.expect_err("refused after a message too");
    assert!(refused.contains("has started"), "{refused}");
    assert_eq!(row(&pool, id).await, Some(("driver".to_string(), true)));
}

/// S3, two named interleavings. (a) A send admitted while the set holds
/// the turn lock, before its insert: the turn waits for the lock, and its
/// first request is made with the persona applied and its rules in force.
/// (b) A send while the row is inserted but not yet applied: refused.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_send_racing_a_set_runs_under_the_persona(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids");
    // Each request records whether the persona was applied when it came.
    let applied_at_request = Arc::new(Mutex::new(Vec::new()));
    let (seen, probe) = (applied_at_request.clone(), pool.clone());
    let requests = mock_with(&pool, move |_| {
        let (seen, probe) = (seen.clone(), probe.clone());
        Box::pin(async move {
            let applied: Vec<bool> =
                sqlx::query_scalar("SELECT applied_at IS NOT NULL FROM top_level_personas").fetch_all(&probe).await.expect("probe");
            seen.lock().expect("seen").push(applied);
            text_reply("ok")
        })
    })
    .await;
    let driver = persona_md("driver", "smelt:\n  tools:\n    deny: [write_file]\n");
    let id = conversation_with(&pool, Some(true), &[("driver", driver.clone())]).await;

    let (reached, release) = test_hooks::pause(test_hooks::TOP_LEVEL_AFTER_READ, id);
    let setting = tokio::spawn({
        let pool = pool.clone();
        async move { set(&pool, id, "app:driver").await }
    });
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the set reads").expect("paused");
    crate::turn::start_turn(pool.clone(), id, "Hi.".to_string()).await.expect("admitted: no row yet");
    release.send(()).expect("release the set");
    let set_to = tokio::time::timeout(Duration::from_secs(10), setting).await.expect("the set ends").expect("joined");
    assert!(set_to.expect("the set").applied);
    let turn_request = || requests.lock().expect("log").iter().find(|r| r["tools"].is_array()).cloned();
    eventually("the turn's first request", || async { turn_request().is_some() }).await;
    let first = turn_request().expect("the turn's request");
    let tools: Vec<String> = first["tools"].as_array().expect("tools").iter().map(|t| t["name"].as_str().unwrap_or("").to_string()).collect();
    assert!(!tools.iter().any(|t| t == "write_file"), "the persona's deny is in force: {tools:?}");
    assert!(first["system"].to_string().contains("# Persona"), "its persona is in the prompt");
    let seen = applied_at_request.lock().expect("seen").clone();
    assert!(!seen.is_empty() && seen.iter().all(|rows| *rows == [true]), "applied before every call: {seen:?}");
    eventually("the turn ends", || async { !crate::turn::turn_running(id) }).await;

    let other = conversation_with(&pool, Some(true), &[("driver", driver)]).await;
    let (reached, release) = test_hooks::pause(test_hooks::TOP_LEVEL_AFTER_INSERT, other);
    let setting = tokio::spawn({
        let pool = pool.clone();
        async move { set(&pool, other, "app:driver").await }
    });
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the set inserts").expect("paused");
    let refused = crate::turn::start_turn(pool.clone(), other, "Hi.".to_string()).await.expect_err("refused while unapplied");
    assert!(refused.to_string().contains("still being applied"), "{refused}");
    release.send(()).expect("release the set");
    tokio::time::timeout(Duration::from_secs(10), setting).await.expect("the set ends").expect("joined").expect("the set");
    assert!(db::list_messages(&pool, other).await.expect("messages").is_empty(), "the refused send saved nothing");
}

/// S4: an unapplied row (a set a restart cut off) refuses turns until the
/// startup sweep deletes it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_unapplied_persona_refuses_turns_and_the_sweep_clears_it(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let id = db::create_conversation(&pool).await.expect("a conversation").id;
    sqlx::query(
        "INSERT INTO top_level_personas (conversation_id, persona_name, persona_path, persona_body, persona_settings, credentials)
         VALUES ($1, 'driver', '/workspace/app/.agents/personas/driver.md', 'Drive.', '{}', '{}')",
    )
    .bind(id)
    .execute(&pool)
    .await
    .expect("an unapplied row");
    let refused = crate::turn::start_turn(pool.clone(), id, "Hi.".to_string()).await.expect_err("refused");
    assert!(refused.to_string().contains("still being applied"), "{refused}");
    assert!(!admits_notices(&pool, id).await, "no notice runs a turn either");
    restart(&pool, &[id]).await;
    assert_eq!(row(&pool, id).await, None, "the sweep deleted it");
    crate::turn::start_turn(pool.clone(), id, "Hi.".to_string()).await.expect("admitted now");
    eventually("the turn ends", || async { !crate::turn::turn_running(id) }).await;
}

/// S5: a child's persona is start_agent's to set.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_childs_persona_cant_be_set_this_way(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids");
    let root = conversation_with(&pool, Some(true), &[("driver", persona_md("driver", ""))]).await;
    let child = active_child(&pool, root).await;
    let refused = set(&pool, child, "app:driver").await.expect_err("refused");
    assert!(refused.contains("start_agent"), "{refused}");
    assert_eq!(row(&pool, child).await, None);
}

/// T1 and T2: an undecided or declined remote lists no option and nothing
/// of its files, and its persona can't be set.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_undecided_or_declined_remote_neither_lists_nor_sets_its_personas(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids");
    for (trust, note) in [(None, "Trust https://example.com/team/app.git"), (Some(false), "declined to trust")] {
        let id = conversation_with(&pool, trust, &[("driver", persona_md("driver", ""))]).await;
        let listed = crate::agents::top_level::get_persona(&pool, id).await.expect("the listing");
        assert!(listed.options.is_empty(), "{trust:?}: {listed:?}");
        assert!(listed.notes.iter().any(|n| n.contains(note)), "{trust:?}: {listed:?}");
        let shown = format!("{listed:?}");
        assert!(!shown.contains("marker-driver-inside"), "{trust:?}: nothing of the file: {shown}");
        let refused = set(&pool, id, "app:driver").await.expect_err("refused");
        assert!(refused.contains("doesn't trust"), "{trust:?}: {refused}");
        assert!(!refused.contains("marker-driver-inside"), "{refused}");
        assert_eq!(row(&pool, id).await, None);
        // A trusted remote lists it, description and all.
        db::set_repo_trust(&pool, KEY, true).await.expect("trust");
        let listed = crate::agents::top_level::get_persona(&pool, id).await.expect("the listing");
        assert_eq!(listed.options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), ["app:driver"]);
        assert!(listed.options[0].description.as_deref().is_some_and(|d| d.contains("marker-driver-inside")));
        db::delete_repo_trust(&pool, KEY).await.expect("forget");
    }
}

/// T3: trust forgotten between the file's read and the transaction: the
/// transaction's own re-read refuses it, and nothing is written.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_trust_forgotten_between_the_read_and_the_insert_is_refused(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids");
    let id = conversation_with(&pool, Some(true), &[("driver", persona_md("driver", ""))]).await;
    let (reached, release) = test_hooks::pause(test_hooks::TOP_LEVEL_AFTER_READ, id);
    let setting = tokio::spawn({
        let pool = pool.clone();
        async move { set(&pool, id, "app:driver").await }
    });
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the set reads").expect("paused");
    db::delete_repo_trust(&pool, KEY).await.expect("forget");
    release.send(()).expect("release the set");
    let refused = tokio::time::timeout(Duration::from_secs(10), setting).await.expect("ends").expect("joined").expect_err("refused");
    assert!(refused.contains("doesn't trust"), "{refused}");
    assert_eq!(row(&pool, id).await, None);
}

/// Two providers, `Anthropic` (the default, on `default-model`) and
/// `ClaudeCode`, each listing `models`; returns their ids.
async fn providers(pool: &PgPool, anthropic: &[&str], claude_code: &[&str]) -> (i64, i64) {
    let mut ids = Vec::new();
    for (name, kind, models) in [("Anthropic", "anthropic", anthropic), ("ClaudeCode", "claude_cli", claude_code)] {
        let provider = db::create_inference_provider(pool, name, kind, "http://127.0.0.1:9", "api_key", "k", false, None, true, None)
            .await
            .expect("a provider");
        for model in models {
            sqlx::query("INSERT INTO provider_models (provider_id, model) VALUES ($1, $2)")
                .bind(provider.id)
                .bind(model)
                .execute(pool)
                .await
                .expect("a model");
        }
        ids.push(provider.id);
    }
    db::set_default_model(pool, ids[0], "default-model").await.expect("the default");
    (ids[0], ids[1])
}

/// The conversation's chosen provider and model, as stored.
async fn stored_model(pool: &PgPool, id: i64) -> (Option<i64>, Option<String>) {
    sqlx::query_as("SELECT provider_id, model FROM conversations WHERE id = $1").bind(id).fetch_one(pool).await.expect("the row")
}

const OPUS: &str = "smelt:\n  model: claude-opus-5-5\n";

/// M1: the conversation's own provider lists the persona's model: it runs
/// there, though the default's provider lists it too and sorts first.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_persona_model_resolves_on_the_conversations_own_provider(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids");
    let (_, claude_code) = providers(&pool, &["default-model", "claude-opus-5-5"], &["claude-opus-4-x", "claude-opus-5-5"]).await;
    let id = conversation_with(&pool, Some(true), &[("driver", persona_md("driver", OPUS))]).await;
    db::set_conversation_model(&pool, id, claude_code, "claude-opus-4-x").await.expect("on ClaudeCode");
    let mut events = crate::events::subscribe(id);
    let set_to = set(&pool, id, "app:driver").await.expect("the set");
    assert_eq!(stored_model(&pool, id).await, (Some(claude_code), Some("claude-opus-5-5".to_string())));
    assert_eq!((set_to.model.as_deref(), set_to.provider.as_deref()), (Some("claude-opus-5-5"), Some("ClaudeCode")));
    // Open tabs' model pickers are told the model moved.
    let mut told = false;
    while let Ok(event) = events.try_recv() {
        told |= event == crate::events::ConversationEvent::ModelChanged {};
    }
    assert!(told, "the set's model pin publishes ModelChanged");
}

/// M2: the conversation's provider lacks it and the default's has it:
/// refused, loudly, naming both — never rerouted — and nothing written.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_persona_model_the_provider_lacks_is_refused_not_rerouted(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids");
    let (_, claude_code) = providers(&pool, &["default-model", "claude-opus-5-5"], &["claude-opus-4-x"]).await;
    let id = conversation_with(&pool, Some(true), &[("driver", persona_md("driver", OPUS))]).await;
    db::set_conversation_model(&pool, id, claude_code, "claude-opus-4-x").await.expect("on ClaudeCode");
    let refused = set(&pool, id, "app:driver").await.expect_err("refused");
    for named in ["claude-opus-5-5", "`ClaudeCode` (this conversation's provider) doesn't list", "`Anthropic`"] {
        assert!(refused.contains(named), "{named}: {refused}");
    }
    assert_eq!(row(&pool, id).await, None);
    assert_eq!(stored_model(&pool, id).await, (Some(claude_code), Some("claude-opus-4-x".to_string())));
}

/// M3: a conversation on the default model resolves on the default's
/// provider, and is pinned there.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_default_model_conversation_resolves_on_the_default_provider_and_pins_it(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids");
    let (anthropic, _) = providers(&pool, &["default-model", "claude-opus-5-5"], &["claude-opus-5-5"]).await;
    let id = conversation_with(&pool, Some(true), &[("driver", persona_md("driver", OPUS))]).await;
    assert_eq!(stored_model(&pool, id).await, (None, None), "on the default");
    set(&pool, id, "app:driver").await.expect("the set");
    assert_eq!(stored_model(&pool, id).await, (Some(anthropic), Some("claude-opus-5-5".to_string())));
}

/// M4: a persona without `smelt.model` leaves the conversation's model be.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_persona_without_a_model_keeps_the_conversations(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids");
    let (_, claude_code) = providers(&pool, &["default-model"], &["claude-opus-4-x"]).await;
    let id = conversation_with(&pool, Some(true), &[("driver", persona_md("driver", ""))]).await;
    db::set_conversation_model(&pool, id, claude_code, "claude-opus-4-x").await.expect("on ClaudeCode");
    set(&pool, id, "app:driver").await.expect("the set");
    assert_eq!(stored_model(&pool, id).await, (Some(claude_code), Some("claude-opus-4-x".to_string())));
}
