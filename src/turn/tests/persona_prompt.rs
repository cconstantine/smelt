//! T9 (SME-506): the `smelt` keys a persona's file listed in
//! `ignore_if_unknown` and this smelt skipped are said in the seat's own
//! `# Persona` section — the one channel that reaches a top-level seat,
//! which has no parent. A snapshot from before the ticket has no list.

use super::compact_at::set_persona_settings;
use super::prompt::prompt_env;
use super::*;

const LINE: &str = "- This smelt doesn't know `smelt.compact_later` from your persona file, so it was ignored.";

fn persona(ignored: &[&str]) -> crate::db::AgentPersona {
    crate::db::AgentPersona {
        persona_path: "/workspace/app/.agents/personas/developer.md".to_string(),
        persona_commit: Some("b7744764abcdef".to_string()),
        persona_body: "Develop.".to_string(),
        ignored: ignored.iter().map(|k| k.to_string()).collect(),
    }
}

#[test]
fn test_a_child_and_a_top_level_seat_are_told_of_their_ignored_keys() {
    let child = PromptEnvironment {
        agent: Some(AgentPrompt { id: 7, parent_id: 42, persona: Some(persona(&["compact_later"])) }),
        ..prompt_env()
    };
    let top = PromptEnvironment { top_level_persona: Some(persona(&["compact_later"])), ..prompt_env() };
    for env in [child, top] {
        let prompt = system_prompt(&env);
        let section = &prompt[prompt.find("# Persona").expect("a Persona section")..];
        assert!(section.contains(LINE), "{prompt}");
    }
    let none = PromptEnvironment { top_level_persona: Some(persona(&[])), ..prompt_env() };
    assert!(!system_prompt(&none).contains("so it was ignored"), "no ignored key, no line");
}

/// The list comes from `persona_settings` on either arm of
/// `agent_persona`; a row written before SME-506 has none, and reads.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_agent_persona_reads_the_ignored_keys_from_either_snapshot(pool: PgPool) {
    let top = db::create_conversation(&pool).await.expect("a conversation").id;
    set_persona_settings(&pool, top, serde_json::json!({"ignored": ["compact_later"]})).await;
    let read = db::agent_persona(&pool, top).await.expect("read").expect("a persona");
    assert_eq!(read.ignored, vec!["compact_later".to_string()]);

    let child = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('c', $1) RETURNING id")
        .bind(top)
        .fetch_one(&pool)
        .await
        .expect("a child conversation");
    sqlx::query(
        "INSERT INTO agents (conversation_id, root_id, depth, brief, state, started_at, persona_path, persona_body, persona_settings)
         VALUES ($1, $2, 1, 'b', 'active', now(), '/workspace/app/.agents/personas/developer.md', 'Develop.', $3)",
    )
    .bind(child)
    .bind(top)
    .bind(serde_json::json!({"ignored": ["k1", "k2"]}))
    .execute(&pool)
    .await
    .expect("an agent row");
    let read = db::agent_persona(&pool, child).await.expect("read").expect("a persona");
    assert_eq!(read.ignored, vec!["k1".to_string(), "k2".to_string()]);

    let old = db::create_conversation(&pool).await.expect("a conversation").id;
    set_persona_settings(&pool, old, serde_json::json!({"compact_at": null})).await;
    let read = db::agent_persona(&pool, old).await.expect("an old snapshot reads").expect("a persona");
    assert!(read.ignored.is_empty());
}
