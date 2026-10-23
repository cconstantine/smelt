//! `agents` and `agent_messages` (SME-236).

use super::*;

/// A wake finds work in an undelivered report or notice, as in a finished
/// command (SME-218): otherwise a report queued to an idle parent would
/// never be drained.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_wake_has_work_sees_an_undelivered_agent_message(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    assert!(!wake_has_work(&pool, conversation.id).await.expect("wake_has_work"), "nothing queued yet");
    let mut conn = pool.acquire().await.expect("a connection");
    queue_agent_message(&mut conn, conversation.id, None, "agent #9", KIND_REPORT, "done")
        .await
        .expect("queue a report");
    assert!(wake_has_work(&pool, conversation.id).await.expect("wake_has_work"), "an undelivered report is work");
    sqlx::query("UPDATE agent_messages SET delivered_at = now()")
        .execute(&pool)
        .await
        .expect("mark it delivered");
    assert!(!wake_has_work(&pool, conversation.id).await.expect("wake_has_work"), "a delivered one isn't");
}

/// An agent row under `parent`, in `state`, with a persona when given.
async fn agent(pool: &PgPool, parent: i64, state: &str, persona: Option<(&str, &str)>) -> i64 {
    let child = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ($1, $2) RETURNING id")
        .bind(format!("agent of {parent}"))
        .bind(parent)
        .fetch_one(pool)
        .await
        .expect("a child row");
    sqlx::query(
        "INSERT INTO agents (conversation_id, root_id, depth, brief, persona_name, persona_path, state)
         VALUES ($1, $2, 1, 'a brief', $3, $4, $5)",
    )
    .bind(child)
    .bind(parent)
    .bind(persona.map(|(name, _)| name))
    .bind(persona.map(|(_, path)| path))
    .bind(state)
    .execute(pool)
    .await
    .expect("its agent row");
    child
}

/// An AGENTS.md trust card waiting in `conversation_id`.
async fn trust_card(pool: &PgPool, conversation_id: i64) {
    let repo = create_conversation_repo(pool, conversation_id, "https://example.com/r.git", "example.com/r", None, "r")
        .await
        .expect("a repo row");
    let file = InstructionsFile { content: "Rules.".to_string(), file_bytes: 6, hash: "h".to_string(), commit: None };
    request_instruction(pool, conversation_id, repo.id, "AGENTS.md", &file).await.expect("a card");
}

/// A question in `conversation_id` asked of `asked_of`, unanswered.
async fn question(pool: &PgPool, conversation_id: i64, asked_of: &str) {
    sqlx::query("INSERT INTO pending_questions (conversation_id, tool_use_id, questions, asked_of) VALUES ($1, 'toolu_q', '[]', $2)")
        .bind(conversation_id)
        .bind(asked_of)
        .execute(pool)
        .await
        .expect("a question");
}

/// SME-246: the tree has every conversation, most recently active first,
/// with an agent's state and persona and what it waits on.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_conversation_tree_has_each_agents_state_persona_and_waits(pool: PgPool) {
    let root = test_conversation(&pool).await.id;
    let path = "/workspace/smelt/.agents/personas/developer.md";
    let asks_user = agent(&pool, root, AGENT_ACTIVE, Some(("developer", path))).await;
    let asks_parent = agent(&pool, root, AGENT_ACTIVE, None).await;
    let has_card = agent(&pool, asks_user, AGENT_FINISHED, None).await;
    let other = test_conversation(&pool).await.id;
    question(&pool, asks_user, ASKED_OF_USER).await;
    question(&pool, asks_parent, ASKED_OF_PARENT).await;
    trust_card(&pool, has_card).await;
    // Most recently active first: `other`, then the rest by their bump.
    for (id, minutes) in [(root, 5), (asks_user, 4), (asks_parent, 3), (has_card, 2), (other, 1)] {
        sqlx::query("UPDATE conversations SET updated_at = now() - make_interval(mins => $2) WHERE id = $1")
            .bind(id)
            .bind(minutes)
            .execute(&pool)
            .await
            .expect("age it");
    }

    let rows = conversation_tree(&pool).await.expect("the tree");
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![other, has_card, asks_parent, asks_user, root]);
    let by_id = |id: i64| rows.iter().find(|r| r.id == id).expect("listed").clone();
    let user_row = by_id(root);
    assert_eq!((user_row.state, user_row.parent_id, user_row.waiting_for_user), (None, None, false));
    let asking = by_id(asks_user);
    assert_eq!(asking.parent_id, Some(root));
    assert_eq!(asking.state.as_deref(), Some(AGENT_ACTIVE));
    assert_eq!((asking.persona_name.as_deref(), asking.persona_path.as_deref()), (Some("developer"), Some(path)));
    assert_eq!((asking.waiting_for_user, asking.waiting_for_parent), (true, false));
    let parent_asked = by_id(asks_parent);
    assert_eq!((parent_asked.waiting_for_user, parent_asked.waiting_for_parent), (false, true));
    let carded = by_id(has_card);
    assert_eq!(carded.state.as_deref(), Some(AGENT_FINISHED));
    assert_eq!((carded.parent_id, carded.waiting_for_user, carded.waiting_for_parent), (Some(asks_user), true, false));
}
