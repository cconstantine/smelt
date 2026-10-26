//! `start_agent` under SME-241's rules: who may start one (the depth
//! ceiling), what a child inherits (root, depth, the parent's chain),
//! and that a later persona edit changes nothing already stored.

use super::repo::{FakeCheckout, root_with_checkout, start_on};
use super::*;

const DEVELOPER: &str = ".agents/personas/developer.md";
const REVIEWER: &str = ".agents/personas/reviewer.md";

/// The serialized entry a `smelt.tools` block of `class`/`deny` makes.
fn dev_entry() -> serde_json::Value {
    serde_json::json!({"class": "read-only", "allow": [], "deny": ["stop_agent"]})
}

fn dev_rules() -> String {
    "---\nname: developer\ndescription: Works one ticket.\nsmelt:\n  tools:\n    class: read-only\n    deny:\n      - stop_agent\n---\nWork.\n".to_string()
}

fn reviewer_rules() -> String {
    "---\nname: reviewer\ndescription: Reviews one diff.\nsmelt:\n  tools:\n    class: no-destructive\n    deny:\n      - browser_click\n---\nReview.\n".to_string()
}

fn planner_rules() -> String {
    "---\nname: planner\ndescription: Plans one ticket.\nsmelt:\n  tools:\n    class: no-destructive\n---\nPlan.\n".to_string()
}

/// The edited file's entry: the same name, wider rules.
fn dev_rules_widened() -> String {
    "---\nname: developer\ndescription: Works one ticket.\nsmelt:\n  tools:\n    allow:\n      - stop_agent\n---\nWork.\n".to_string()
}

/// A conversation under `parent` with an `active` agent row at `depth`,
/// rooted at `root`: the direct-SQL stand-in for a nested agent.
async fn agent_at_depth(pool: &PgPool, parent: i64, root: i64, depth: i32) -> i64 {
    let child = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('deep', $1) RETURNING id")
        .bind(parent)
        .fetch_one(pool)
        .await
        .expect("a conversation");
    sqlx::query(
        "INSERT INTO agents (conversation_id, root_id, depth, brief, state, started_at) VALUES ($1, $2, $3, 'b', 'active', now())",
    )
    .bind(child)
    .bind(root)
    .bind(depth)
    .execute(pool)
    .await
    .expect("an agent row");
    child
}

/// The stored `rule_chain`, as JSON.
async fn chain(pool: &PgPool, id: i64) -> Option<serde_json::Value> {
    sqlx::query_scalar::<_, Option<serde_json::Value>>("SELECT rule_chain FROM agents WHERE conversation_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the chain column")
}

/// The agent row's `depth` and `root_id`.
async fn depth_and_root(pool: &PgPool, id: i64) -> (i32, i64) {
    sqlx::query_as::<_, (i32, i64)>("SELECT depth, root_id FROM agents WHERE conversation_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the agent row")
}

#[test]
fn test_smelt_agent_depth_parses_and_falls_back() {
    use crate::agents::agent_depth_from;
    assert_eq!(agent_depth_from(None), 5, "the default");
    assert_eq!(agent_depth_from(Some(String::new())), 5, "an empty setting is no setting");
    assert_eq!(agent_depth_from(Some("2".to_string())), 2, "an operator may narrow it");
    for bad in ["x", "-1", "0"] {
        assert_eq!(agent_depth_from(Some(bad.to_string())), 5, "{bad:?} isn't a depth: the default stands");
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_child_is_offered_start_agent_and_makes_a_grandchild_under_it(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = start_child(&pool, root, "Work.").await;
    eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;

    let offered = crate::anthropic::tools::tool_definitions(&pool, child).await;
    assert!(offered.iter().any(|d| d.name == "start_agent"), "start_agent is an ordinary tool now");

    let result = crate::anthropic::tools::execute(&pool, child, "toolu_s", "start_agent", &serde_json::json!({"brief": "Grand."}))
        .await
        .expect("a child may start one");
    let grand = child_of(&pool, child, &result).await;
    assert_eq!(depth_and_root(&pool, grand).await, (2, root), "depth nests and the root stays the top");
    assert_eq!(chain(&pool, grand).await, None, "no rules on this line: the chain stays NULL");
    eventually("every turn is over", || async {
        !crate::turn::turn_running(root) && !crate::turn::turn_running(child) && !crate::turn::turn_running(grand)
    })
    .await;
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_agent_at_the_depth_ceiling_is_refused_start_agent(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let mut caller = root;
    for depth in 1..=crate::agents::DEFAULT_AGENT_DEPTH {
        caller = agent_at_depth(&pool, caller, root, depth).await;
    }
    let result =
        crate::anthropic::tools::execute(&pool, caller, "toolu_s", "start_agent", &serde_json::json!({"brief": "go"})).await;
    assert_eq!(
        result,
        Err(format!(
            "start_agent can't go deeper than {} levels of agents (you're at depth {})",
            crate::agents::DEFAULT_AGENT_DEPTH,
            crate::agents::DEFAULT_AGENT_DEPTH
        ))
    );
    let children = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM conversations WHERE parent_id = $1")
        .bind(caller)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(children, 0, "the refused call made nothing");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_each_generation_inherits_the_chain_and_adds_its_own(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let fake = Arc::new(FakeCheckout {
        files: Mutex::new(
            [
                (".agents/personas/planner.md".to_string(), planner_rules()),
                (REVIEWER.to_string(), reviewer_rules()),
                (DEVELOPER.to_string(), dev_rules()),
            ]
            .into_iter()
            .collect(),
        ),
        ..Default::default()
    });
    let root = root_with_checkout(&pool, Some(true), fake.clone()).await;
    let input = |persona: &str| serde_json::json!({"brief": "x", "repo": "/workspace/app", "ref": "v2", "persona": persona});

    let first = child_of(&pool, root, &start_on(&pool, root, input("planner")).await.expect("started")).await;
    eventually("the first child goes active", || async { state(&pool, first).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
    let planner = serde_json::json!({"class": "no-destructive", "allow": [], "deny": []});
    assert_eq!(chain(&pool, first).await, Some(serde_json::json!([planner])), "its persona's entry, and only it");

    // Each child starts one of its own: its own start cloned its checkout
    // ready, and the same fake answers for it. A `read-only` line couldn't
    // nest (its chain denies the writing `start_agent` — the design's
    // worked example), so the line narrows only at the third generation.
    test_fakes::register(first, fake.clone());
    let reviewer = serde_json::json!({"class": "no-destructive", "allow": [], "deny": ["browser_click"]});
    let second = child_of(&pool, first, &start_on(&pool, first, input("reviewer")).await.expect("started")).await;
    eventually("the second child goes active", || async { state(&pool, second).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
    assert_eq!(chain(&pool, second).await, Some(serde_json::json!([planner, reviewer])), "inherited first, own last");

    test_fakes::register(second, fake.clone());
    let third = child_of(&pool, second, &start_on(&pool, second, input("developer")).await.expect("started")).await;
    eventually("the third child goes active", || async { state(&pool, third).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
    assert_eq!(
        chain(&pool, third).await,
        Some(serde_json::json!([planner, reviewer, dev_entry()])),
        "three entries, its ancestors' order kept"
    );
    assert_eq!(depth_and_root(&pool, third).await, (3, root), "the root stays the top of the line");

    // The chain still bites three generations down: `stop_agent`, writing,
    // is gone under the last entry's ceiling, and `browser_click`, named
    // by the second entry's deny, never came back.
    let offered = crate::anthropic::tools::tool_definitions(&pool, third).await;
    let names: Vec<&str> = offered.iter().map(|d| d.name.as_str()).collect();
    assert!(!names.contains(&"stop_agent"), "no entry can lift an earlier ceiling: {names:?}");
    assert!(!names.contains(&"browser_click"), "a deny holds down the line: {names:?}");
    assert!(names.contains(&"browser_navigate"), "the untouched rest stays: {names:?}");

    eventually("every turn is over", || async {
        !crate::turn::turn_running(root) && !crate::turn::turn_running(first) && !crate::turn::turn_running(second)
    })
    .await;
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_editing_the_persona_after_a_start_changes_no_stored_chain(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let fake = Arc::new(FakeCheckout {
        files: Mutex::new([(DEVELOPER.to_string(), dev_rules())].into_iter().collect()),
        ..Default::default()
    });
    let root = root_with_checkout(&pool, Some(true), fake.clone()).await;
    let input = || serde_json::json!({"brief": "x", "repo": "/workspace/app", "ref": "v2", "persona": "developer"});

    let first = child_of(&pool, root, &start_on(&pool, root, input()).await.expect("started")).await;
    eventually("the first child goes active", || async { state(&pool, first).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;

    // The file changes after the start: what's stored is stored.
    fake.files.lock().expect("files").insert(DEVELOPER.to_string(), dev_rules_widened());
    let second = child_of(&pool, root, &start_on(&pool, root, input()).await.expect("started")).await;
    eventually("the second child goes active", || async { state(&pool, second).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;

    assert_eq!(chain(&pool, first).await, Some(serde_json::json!([dev_entry()])), "the edit changed nothing stored");
    assert_eq!(
        chain(&pool, second).await,
        Some(serde_json::json!([{"class": "all", "allow": ["stop_agent"], "deny": []}])),
        "the second child's entry came from the edited file"
    );
    eventually("every turn is over", || async {
        !crate::turn::turn_running(root) && !crate::turn::turn_running(first) && !crate::turn::turn_running(second)
    })
    .await;
}
