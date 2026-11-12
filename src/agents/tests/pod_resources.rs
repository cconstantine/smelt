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

const GATE: &str = "    docker: { memory_request: 1Gi, memory_limit: 2Gi }\n";
const OTHER: &str = "    sandbox: { memory_request: 256Mi, memory_limit: 1Gi }\n";

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
    let expected = serde_json::json!({"docker": {"memory_request": "1Gi", "memory_limit": "2Gi"}});
    eventually("the clone is under way", || async { !fake.targets.lock().expect("targets").is_empty() }).await;
    assert_eq!(pod_resources(&pool, child).await, Some(expected.clone()), "set before the clone's pod");
    hold.notify_one();
    eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
    assert_eq!(pod_resources(&pool, child).await, Some(expected));
    assert!(fake.restarts.lock().expect("restarts").is_empty(), "the same persona: no restart");
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
}

fn gate() -> serde_json::Value {
    serde_json::json!({"docker": {"memory_request": "1Gi", "memory_limit": "2Gi"}})
}

fn other() -> serde_json::Value {
    serde_json::json!({"sandbox": {"memory_request": "256Mi", "memory_limit": "1Gi"}})
}

/// Starts `app:developer` on `fake`'s root (at `git_ref`, when set) and
/// returns the root and the child.
async fn start_developer(pool: &PgPool, fake: Arc<FakeCheckout>, git_ref: Option<&str>) -> (i64, i64) {
    let root = root_with_checkout(pool, Some(true), fake).await;
    let mut input = serde_json::json!({"brief": "x", "persona": "app:developer"});
    if let Some(r) = git_ref {
        input["ref"] = serde_json::json!(r);
    }
    let child = child_of(pool, root, &start_on(pool, root, input).await.expect("started")).await;
    (root, child)
}

async fn turns_over(root: i64, child: i64) {
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
}

/// M1: a ref start whose persona at the ref asks for other room than the
/// parent's tree said drops its fresh storage and clones again, so the
/// pod that runs it is placed with its requests rather than restarted on
/// the node the first pod chose. The parent is told.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_ref_start_whose_persona_differs_is_recloned_before_it_runs(pool: PgPool) {
    setup(&pool, vec![text_reply("ok"), text_reply("ok")]).await;
    let fake = Arc::new(fake_checkout(persona_with(Some(GATE)), Some(persona_with(None))));
    let (root, child) = start_developer(&pool, fake.clone(), Some("v2")).await;
    eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
    assert_eq!(*fake.reprovisions.lock().expect("reprovisions"), vec![child]);
    assert_eq!(*fake.clone_resources.lock().expect("clones"), vec![None, Some(gate())], "the second clone asks for GATE");
    assert!(fake.restarts.lock().expect("restarts").is_empty(), "no restart in place");
    assert_eq!(pod_resources(&pool, child).await, Some(gate()));
    let told = notices(&pool, root).await;
    assert!(told.iter().any(|n| n.contains(&format!("agent #{child}")) && n.contains("made again")), "{told:?}");
    turns_over(root, child).await;
}

/// M1's pre-read: a ref start reads the persona in the parent's tree,
/// best-effort, so the first clone's pod already asks for its room.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_ref_start_reads_the_persona_in_the_parents_tree_first(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let fake = Arc::new(fake_checkout(persona_with(Some(GATE)), Some(persona_with(Some(GATE)))));
    let (root, child) = start_developer(&pool, fake.clone(), Some("v2")).await;
    eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
    assert_eq!(*fake.clone_resources.lock().expect("clones"), vec![Some(gate())]);
    assert!(fake.reprovisions.lock().expect("reprovisions").is_empty());
    assert!(fake.restarts.lock().expect("restarts").is_empty());
    turns_over(root, child).await;
}

/// The pre-read is best-effort: a parent's file that doesn't parse
/// doesn't refuse a ref start, whose persona only has to be at the ref.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_ref_start_whose_parent_cant_read_its_persona_still_starts(pool: PgPool) {
    setup(&pool, vec![text_reply("ok"), text_reply("ok")]).await;
    let fake = Arc::new(fake_checkout(persona_with(Some(GATE)), Some("not a persona".to_string())));
    let (root, child) = start_developer(&pool, fake.clone(), Some("v2")).await;
    eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
    assert_eq!(*fake.reprovisions.lock().expect("reprovisions"), vec![child]);
    assert_eq!(fake.clone_resources.lock().expect("clones").last().cloned(), Some(Some(gate())));
    turns_over(root, child).await;
}

/// The backstop: the ref moved between the two clones and its persona
/// with it, so the snapshot differs again and the pod is restarted in
/// place.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_persona_that_changes_between_clones_restarts_its_pod_in_place(pool: PgPool) {
    setup(&pool, vec![text_reply("ok"), text_reply("ok")]).await;
    let mut fake = fake_checkout(persona_with(Some(GATE)), Some(persona_with(None)));
    fake.files_after_reprovision = Some([(DEVELOPER.to_string(), persona_with(Some(OTHER)))].into());
    let fake = Arc::new(fake);
    let (root, child) = start_developer(&pool, fake.clone(), Some("v2")).await;
    eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
    assert_eq!(*fake.reprovisions.lock().expect("reprovisions"), vec![child]);
    assert_eq!(*fake.restarts.lock().expect("restarts"), vec![child]);
    assert_eq!(pod_resources(&pool, child).await, Some(other()));
    turns_over(root, child).await;
}

/// A reprovision that fails fails the start (the Driver's decision: by
/// then the storage may be gone, so there's no restart in place to fall
/// back on), with its error in the report.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_failed_reprovision_fails_the_start(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let mut fake = fake_checkout(persona_with(Some(GATE)), Some(persona_with(None)));
    fake.reprovision_error = Some("the claims are stuck".to_string());
    let fake = Arc::new(fake);
    let (root, child) = start_developer(&pool, fake.clone(), Some("v2")).await;
    eventually("the start fails", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_FAILED) }).await;
    eventually("the parent gets the error as the report", || async {
        transcript(&pool, root).await.iter().any(|(_, text)| text.contains("start failed") && text.contains("the claims are stuck"))
    })
    .await;
    assert!(fake.restarts.lock().expect("restarts").is_empty(), "no fallback to a restart");
    eventually("every turn is over", || async { !crate::turn::turn_running(root) }).await;
}

/// No ref, but the parent's file asks for other room than the child's
/// at the same commit (a dirty tree, say): the child's wins, resources
/// it doesn't name are cleared, and its storage is made again.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_no_ref_child_whose_persona_differs_from_its_parents_is_recloned(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let fake = Arc::new(fake_checkout(persona_with(None), Some(persona_with(Some(OTHER)))));
    let (root, child) = start_developer(&pool, fake.clone(), None).await;
    eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
    assert_eq!(pod_resources(&pool, child).await, None, "the child's persona names none");
    assert_eq!(*fake.reprovisions.lock().expect("reprovisions"), vec![child]);
    assert_eq!(*fake.clone_resources.lock().expect("clones"), vec![Some(other()), None]);
    assert!(fake.restarts.lock().expect("restarts").is_empty());
    turns_over(root, child).await;
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
