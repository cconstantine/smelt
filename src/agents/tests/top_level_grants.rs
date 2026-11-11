//! SME-481: what a top-level persona grants — its tool rules, `own`
//! included (R1, R2), and its credentials, exactly a root child's with the
//! same persona and nothing more (C1–C4) — and what its children narrow
//! from it. The script-MCP reader (R3) is in `sandbox::script_mcp::tests`.

use super::top_level::{conversation_with, persona_md, set};
use super::*;

fn offered(defs: Vec<crate::anthropic::ToolDefinition>) -> Vec<String> {
    defs.into_iter().map(|d| d.name).collect()
}

async fn store_secret(pool: &PgPool, name: &str) {
    sqlx::query("INSERT INTO named_secrets (name, value) VALUES ($1, 'v')").bind(name).execute(pool).await.expect("a secret");
}

async fn store_key(pool: &PgPool) {
    sqlx::query("INSERT INTO ssh_keys (name, public_key, private_key) VALUES ('id_ed25519', 'pub', 'priv')")
        .execute(pool)
        .await
        .expect("a stored key");
}

/// Starts an agent from `caller`, and waits for it to go active.
async fn started_agent(pool: &PgPool, caller: i64, input: serde_json::Value) -> i64 {
    let result = crate::anthropic::tools::execute(pool, caller, "toolu_s", "start_agent", &input).await.expect("started");
    let child = child_of(pool, caller, &result).await;
    eventually("the agent goes active", || async { state(pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
    child
}

async fn chain_bit(pool: &PgPool, id: i64) -> Option<Vec<String>> {
    sqlx::query_scalar("SELECT credentials FROM agents WHERE conversation_id = $1").bind(id).fetch_one(pool).await.expect("the row")
}

/// R1: the persona's rules bind the top-level conversation at the offer
/// and at the call.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_top_level_persona_is_ruled_at_offer_and_call(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let reviewer = persona_md("reviewer", "smelt:\n  tools:\n    class: read-only\n    deny: [write_file]\n");
    let id = conversation_with(&pool, Some(true), &[("reviewer", reviewer)]).await;
    let before = offered(crate::anthropic::tools::tool_definitions(&pool, id).await);
    assert!(before.iter().any(|n| n == "write_file"), "allow-all before the set: {before:?}");
    set(&pool, id, "app:reviewer").await.expect("the set");
    let after = offered(crate::anthropic::tools::tool_definitions(&pool, id).await);
    assert!(!after.iter().any(|n| n == "write_file" || n == "start_agent"), "{after:?}");
    let input = serde_json::json!({"path": "/workspace/x", "content": "x"});
    let refused = crate::anthropic::tools::execute(&pool, id, "toolu_w", "write_file", &input).await.expect_err("refused");
    assert!(refused.contains("write_file"), "{refused}");
}

/// R2: `own` binds the top-level conversation and not its agents; the
/// cascading part binds both.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_own_binds_the_top_level_and_not_its_agents(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let driver = persona_md("driver", "smelt:\n  tools:\n    deny: [create_volume]\n    own:\n      deny: [write_file]\n");
    let id = conversation_with(&pool, Some(true), &[("driver", driver)]).await;
    set(&pool, id, "app:driver").await.expect("the set");
    let top = offered(crate::anthropic::tools::tool_definitions(&pool, id).await);
    assert!(!top.iter().any(|n| n == "write_file" || n == "create_volume"), "{top:?}");
    let input = serde_json::json!({"path": "/workspace/x", "content": "x"});
    let refused = crate::anthropic::tools::execute(&pool, id, "toolu_w", "write_file", &input).await.expect_err("refused");
    assert!(refused.contains("binds this conversation and not its agents"), "{refused}");
    let seat = started_agent(&pool, id, serde_json::json!({"brief": "Work."})).await;
    let below = offered(crate::anthropic::tools::tool_definitions(&pool, seat).await);
    assert!(below.iter().any(|n| n == "write_file"), "its agent may write: {below:?}");
    assert!(!below.iter().any(|n| n == "create_volume"), "the cascading deny holds: {below:?}");
    eventually("every turn is over", || async { !crate::turn::turn_running(id) && !crate::turn::turn_running(seat) }).await;
}

/// C1: a top-level persona gets exactly what a child of a persona-less
/// root with the same persona gets: the key it lists, the secret it lists
/// and the store backs, and never another stored secret.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_top_level_persona_gets_exactly_a_root_childs_credentials(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    store_secret(&pool, "deploy_kubeconfig").await;
    store_secret(&pool, "other").await;
    store_key(&pool).await;
    let deployer = persona_md("deployer", "smelt:\n  credentials: [deploy_kubeconfig, git-ssh]\n");
    let top = conversation_with(&pool, Some(true), &[("deployer", deployer.clone())]).await;
    set(&pool, top, "app:deployer").await.expect("the set");
    let root = conversation_with(&pool, Some(true), &[("deployer", deployer)]).await;
    let child = started_agent(&pool, root, serde_json::json!({"brief": "x", "persona": "app:deployer", "ref": "main"})).await;
    let expected = crate::credentials::PodCredentials { git_ssh: true, secrets: vec!["deploy_kubeconfig".to_string()] };
    assert_eq!(crate::credentials::for_conversation(&pool, child).await.expect("the child's"), expected);
    assert_eq!(crate::credentials::for_conversation(&pool, top).await.expect("the top level's"), expected);
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
}

/// C2: a persona that doesn't list `git-ssh` takes away the key the
/// persona-less root had, and its prompt says so.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_top_level_persona_without_git_ssh_has_no_key(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    store_key(&pool).await;
    let id = conversation_with(&pool, Some(true), &[("reviewer", persona_md("reviewer", "smelt:\n  credentials: []\n"))]).await;
    assert!(crate::credentials::for_conversation(&pool, id).await.expect("before").git_ssh, "the root has the key");
    set(&pool, id, "app:reviewer").await.expect("the set");
    let prompt = crate::credentials::for_prompt(&pool, id).await.expect("the prompt's");
    assert!(!prompt.granted.git_ssh && prompt.key_name.is_none(), "{prompt:?}");
    let env = crate::turn::prompt_environment(&pool, id, "m").await;
    assert!(crate::turn::system_prompt(&env).contains("- No git credential"), "the prompt says so");
}

/// C3: a child of a keyless top-level persona gets no key, though its own
/// persona lists `git-ssh`: its chain bit is seeded empty.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_child_of_a_keyless_top_level_persona_gets_no_key(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    store_key(&pool).await;
    let personas = [
        ("reviewer", persona_md("reviewer", "smelt:\n  credentials: []\n")),
        ("developer", persona_md("developer", "smelt:\n  credentials: [git-ssh]\n")),
    ];
    let id = conversation_with(&pool, Some(true), &personas).await;
    set(&pool, id, "app:reviewer").await.expect("the set");
    let child = started_agent(&pool, id, serde_json::json!({"brief": "x", "persona": "app:developer", "ref": "main"})).await;
    assert_eq!(chain_bit(&pool, child).await, Some(vec![]));
    assert!(!crate::credentials::for_conversation(&pool, child).await.expect("the child's").git_ssh);
    eventually("every turn is over", || async { !crate::turn::turn_running(id) && !crate::turn::turn_running(child) }).await;
}

/// C4: a top-level persona's named secret is its own: a persona-less child
/// of it gets none of it (Rule S is the snapshot's alone), and the key
/// passes down as for any agent.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_top_level_pm_doesnt_pass_its_secret_down(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    store_secret(&pool, "linear-api-key").await;
    store_key(&pool).await;
    let pm = persona_md("pm", "smelt:\n  credentials: [linear-api-key, git-ssh]\n");
    let id = conversation_with(&pool, Some(true), &[("pm", pm)]).await;
    set(&pool, id, "app:pm").await.expect("the set");
    assert_eq!(crate::credentials::for_conversation(&pool, id).await.expect("its own").secrets, ["linear-api-key"]);
    let child = started_agent(&pool, id, serde_json::json!({"brief": "x"})).await;
    let below = crate::credentials::for_conversation(&pool, child).await.expect("the child's");
    assert_eq!(below, crate::credentials::PodCredentials { git_ssh: true, secrets: vec![] });
    eventually("every turn is over", || async { !crate::turn::turn_running(id) && !crate::turn::turn_running(child) }).await;
}

/// SME-484's `smelt.resources` at the top level: the set makes the
/// conversation's `pod_resources` the persona's and restarts its pod (the
/// attach's) with them before it's applied, as a child's start puts its own
/// right; a persona naming none, on a pod with none, restarts nothing.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_top_level_personas_resources_reach_its_pod(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let builder = persona_md("builder", "smelt:\n  resources:\n    docker: { memory_request: 1Gi, memory_limit: 2Gi }\n");
    let plain = persona_md("plain", "");
    let fake = Arc::new(super::repo::FakeCheckout {
        parent_personas: vec![".agents/personas/builder.md".to_string(), ".agents/personas/plain.md".to_string()],
        files: Mutex::new(
            [(".agents/personas/builder.md".to_string(), builder), (".agents/personas/plain.md".to_string(), plain)].into(),
        ),
        ..Default::default()
    });
    let id = super::repo::root_with_checkout(&pool, Some(true), fake.clone()).await;
    set(&pool, id, "app:builder").await.expect("the set");
    let wanted = serde_json::json!({"docker": {"memory_request": "1Gi", "memory_limit": "2Gi"}});
    assert_eq!(db::conversation_pod_resources(&pool, id).await.expect("read"), Some(wanted));
    assert_eq!(*fake.restarts.lock().expect("restarts"), vec![id], "its pod is restarted with them");

    let other = super::repo::root_with_checkout(&pool, Some(true), fake.clone()).await;
    set(&pool, other, "app:plain").await.expect("the set");
    assert_eq!(db::conversation_pod_resources(&pool, other).await.expect("read"), None);
    assert_eq!(*fake.restarts.lock().expect("restarts"), vec![id], "nothing to change, no restart");
}
