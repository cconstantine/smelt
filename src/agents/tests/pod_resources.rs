//! SME-484: a child's persona resources reach its conversation's
//! `pod_resources` — read in the parent's checkout before the clone pod is
//! made, and put right from the child's own snapshot when they differ.

use super::repo::{root_with_checkout, start_on, FakeCheckout, COMMIT};
use super::*;

const DEVELOPER: &str = ".agents/personas/developer.md";

fn persona_with(resources: Option<&str>) -> String {
    let smelt = resources.map(|r| format!("smelt:\n  resources:\n{r}")).unwrap_or_default();
    format!("---\nname: developer\ndescription: Works one ticket.\n{smelt}---\nWork.\n")
}

const GATE: &str = "    docker: { memory_request: 1Gi }\n";
const OTHER: &str = "    sandbox: { memory_request: 256Mi }\n";

fn fake_checkout(child_file: String, parent_file: Option<String>) -> FakeCheckout {
    FakeCheckout {
        agents_md: Some("Project rules.".to_string()),
        parent: Some(Ok((COMMIT.to_string(), "main".to_string()))),
        parent_personas: vec![DEVELOPER.to_string()],
        files: Mutex::new([(DEVELOPER.to_string(), child_file)].into()),
        parent_files: Mutex::new(parent_file.into_iter().map(|f| (DEVELOPER.to_string(), f)).collect()),
        ..Default::default()
    }
}

async fn pod_resources(pool: &PgPool, id: i64) -> Option<serde_json::Value> {
    db::conversation_pod_resources(pool, id).await.expect("read")
}

async fn notices(pool: &PgPool, root: i64) -> Vec<String> {
    sqlx::query_scalar("SELECT body FROM agent_messages WHERE to_conversation_id = $1 AND kind = 'notice'")
        .bind(root)
        .fetch_all(pool)
        .await
        .expect("notices")
}

/// No ref: the persona read in the parent's checkout is on the child's
/// row before its clone (which starts its first pod) runs, and the
/// snapshot, the same file, changes nothing: no restart.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_no_ref_childs_resources_are_set_before_its_clone_from_the_parents_persona(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let hold = Arc::new(tokio::sync::Notify::new());
    let mut fake = fake_checkout(persona_with(Some(GATE)), None);
    fake.hold_clone = Some(hold.clone());
    let fake = Arc::new(fake);
    let root = root_with_checkout(&pool, Some(true), fake.clone()).await;
    let result = start_on(&pool, root, serde_json::json!({"brief": "x", "persona": "app:developer"})).await.expect("started");
    let child = child_of(&pool, root, &result).await;
    let expected = serde_json::json!({"docker": {"memory_request": "1Gi"}});
    eventually("the clone is under way", || async { !fake.targets.lock().expect("targets").is_empty() }).await;
    assert_eq!(pod_resources(&pool, child).await, Some(expected.clone()), "set before the clone's pod");
    hold.notify_one();
    eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
    assert_eq!(pod_resources(&pool, child).await, Some(expected));
    assert!(fake.restarts.lock().expect("restarts").is_empty(), "the same persona: no restart");
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
}

/// A ref start reads nothing in the parent: the snapshot's resources are
/// written at the start and the clone's pod restarted with them, and the
/// parent is told.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_childs_snapshot_that_differs_rewrites_its_resources_and_restarts_its_pod(pool: PgPool) {
    setup(&pool, vec![text_reply("ok"), text_reply("ok")]).await;
    let fake = Arc::new(fake_checkout(persona_with(Some(GATE)), None));
    let root = root_with_checkout(&pool, Some(true), fake.clone()).await;
    let input = serde_json::json!({"brief": "x", "persona": "app:developer", "ref": "v2"});
    let child = child_of(&pool, root, &start_on(&pool, root, input).await.expect("started")).await;
    eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
    assert_eq!(pod_resources(&pool, child).await, Some(serde_json::json!({"docker": {"memory_request": "1Gi"}})));
    assert_eq!(*fake.restarts.lock().expect("restarts"), vec![child]);
    let told = notices(&pool, root).await;
    assert!(told.iter().any(|n| n.contains(&format!("agent #{child}")) && n.contains("restarted")), "{told:?}");

    // The parent's file asks for other room than the child's at its ref:
    // the child's wins, and resources it no longer names are cleared.
    let parent_says = fake_checkout(persona_with(None), Some(persona_with(Some(OTHER))));
    let parent_says = Arc::new(parent_says);
    let other_root = root_with_checkout(&pool, Some(true), parent_says.clone()).await;
    let other = child_of(&pool, other_root, &start_on(&pool, other_root, serde_json::json!({"brief": "x", "persona": "app:developer"})).await.expect("started")).await;
    eventually("the child goes active", || async { state(&pool, other).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
    assert_eq!(pod_resources(&pool, other).await, None, "the child's persona names none");
    assert_eq!(*parent_says.restarts.lock().expect("restarts"), vec![other]);
    eventually("every turn is over", || async {
        [root, child, other_root, other].iter().all(|c| !crate::turn::turn_running(*c))
    })
    .await;
}

/// A child with no persona keeps the deployment's defaults.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_child_without_a_persona_has_no_pod_resources(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let fake = Arc::new(fake_checkout(persona_with(Some(GATE)), None));
    let root = root_with_checkout(&pool, Some(true), fake.clone()).await;
    let child = child_of(&pool, root, &start_on(&pool, root, serde_json::json!({"brief": "x", "repo": "/workspace/app"})).await.expect("started")).await;
    eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
    assert_eq!(pod_resources(&pool, child).await, None);
    assert!(fake.restarts.lock().expect("restarts").is_empty());
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
}
