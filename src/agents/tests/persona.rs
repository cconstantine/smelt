//! A child's persona: checked in the parent, read at the child's ref,
//! kept as a snapshot, with its model.

use super::repo::{FakeCheckout, children, root_with_checkout, start_on, COMMIT};
use super::*;

const DEVELOPER: &str = ".agents/personas/developer.md";

fn persona_file(model: Option<&str>, body: &str) -> String {
    let smelt = model.map(|m| format!("smelt:\n  model: {m}\n")).unwrap_or_default();
    format!("---\nname: developer\ndescription: Works one ticket.\n{smelt}---\n{body}")
}

fn fake_with(files: &[(&str, String)]) -> FakeCheckout {
    FakeCheckout {
        agents_md: Some("Project rules.".to_string()),
        files: Mutex::new(files.iter().map(|(p, c)| (p.to_string(), c.clone())).collect()),
        ..Default::default()
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_persona_not_in_the_parents_checkout_is_refused_before_anything_is_written(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let fake = FakeCheckout {
        parent: Some(Ok((COMMIT.to_string(), "main".to_string()))),
        parent_personas: vec![".agents/personas/reviewer.md".to_string()],
        ..Default::default()
    };
    let root = root_with_checkout(&pool, Some(true), Arc::new(fake)).await;
    let refused = start_on(&pool, root, serde_json::json!({"brief": "x", "repo": "/workspace/app", "persona": "developer"}))
        .await
        .expect_err("refused");
    assert_eq!(refused, "no persona `developer` in /workspace/app (its personas: reviewer); list_personas lists them");
    let refused = start_on(&pool, root, serde_json::json!({"brief": "x", "persona": "developer"})).await.expect_err("refused");
    assert_eq!(refused, "persona needs repo: the checkout whose .agents/personas/ it's in");
    let refused = start_on(&pool, root, serde_json::json!({"brief": "x", "repo": "/workspace/app", "persona": "../x"}))
        .await
        .expect_err("refused");
    assert!(refused.contains("isn't a persona name"), "{refused}");
    assert_eq!(children(&pool, root).await, 0);
}

/// With a ref, the persona is read only at that ref; the child runs under
/// it, its links made absolute, and is named with it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_persona_at_the_childs_ref_goes_in_its_prompt(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Done for now.")]).await;
    let fake = Arc::new(fake_with(&[(DEVELOPER, persona_file(None, "Follow [the process](../../docs/process.md).\n"))]));
    let root = root_with_checkout(&pool, Some(true), fake.clone()).await;
    let result = start_on(&pool, root, serde_json::json!({"brief": "x", "repo": "/workspace/app", "ref": "v2", "persona": "developer"}))
        .await
        .expect("started");
    let child = child_of(&pool, root, &result).await;
    assert!(result.contains("(app:developer)"), "{result}");
    eventually("the child's first turn ran", || async { !requests_for(&requests, Some(child)).is_empty() }).await;

    let system = requests_for(&requests, Some(child))[0]["system"].as_str().expect("a system prompt").to_string();
    let persona = system.find("# Persona").expect("a Persona section");
    let project = system.find("Project rules.").expect("its AGENTS.md");
    assert!(persona < project, "the persona comes before the project instructions");
    assert!(
        system.contains(&format!(
            "Your persona, `app:developer` from /workspace/app/.agents/personas/developer.md at {}, read when you started",
            &COMMIT[..7]
        )),
        "{system}"
    );
    assert!(system.contains("Follow [the process](/workspace/app/docs/process.md)."), "links are absolute: {system}");

    // Named with its persona, to its parent.
    eventually("the parent hears its turn ended", || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_messages WHERE to_conversation_id = $1")
            .bind(root)
            .fetch_one(&pool)
            .await
            .expect("count")
            > 0
    })
    .await;
    let from: String = sqlx::query_scalar("SELECT from_label FROM agent_messages WHERE to_conversation_id = $1")
        .bind(root)
        .fetch_one(&pool)
        .await
        .expect("the notice");
    assert_eq!(from, format!("agent #{child} (app:developer)"));
    let listed = crate::anthropic::tools::execute(&pool, root, "toolu_l", "list_agents", &serde_json::json!({}))
        .await
        .expect("list_agents");
    assert!(listed.contains("\"persona\":\"app:developer\""), "{listed}");

    // A later change to the file changes nothing: the snapshot is the row's.
    fake.files.lock().expect("files").insert(DEVELOPER.to_string(), persona_file(None, "Do something else.\n"));
    crate::turn::start_turn(pool.clone(), child, "Again.".to_string()).await.expect("the send");
    eventually("the second turn ran", || async { requests_for(&requests, Some(child)).len() == 2 }).await;
    let second = requests_for(&requests, Some(child))[1]["system"].to_string();
    assert!(second.contains("Follow [the process]") && !second.contains("Do something else."), "{second}");
    eventually("every turn is over", || async { !crate::turn::turn_running(child) && !crate::turn::turn_running(root) }).await;
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_persona_missing_or_broken_at_the_childs_ref_fails_the_start(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = root_with_checkout(&pool, Some(true), Arc::new(fake_with(&[]))).await;
    let result = start_on(&pool, root, serde_json::json!({"brief": "x", "repo": "/workspace/app", "ref": "v2", "persona": "developer"}))
        .await
        .expect("started");
    let missing = child_of(&pool, root, &result).await;
    eventually("the start fails", || async { state(&pool, missing).await.as_deref() == Some(db::AGENT_FAILED) }).await;
    assert!(
        transcript(&pool, missing).await.iter().any(|(_, t)| t.ends_with(&format!(
            "start failed: no persona `developer` in https://example.com/team/app.git at {COMMIT}"
        ))),
        "{:?}",
        transcript(&pool, missing).await
    );

    let broken = "---\nname: developer\ndescription: d\ntools: Read\n---\nx".to_string();
    let other = db::create_conversation(&pool).await.expect("create conversation").id;
    let repo = db::create_conversation_repo(&pool, other, "https://example.com/team/app.git", "example.com/team/app", None, "app")
        .await
        .expect("a repo row");
    db::set_repo_cloned(&pool, repo.id, "main", Some(COMMIT)).await.expect("ready");
    test_fakes::register(other, Arc::new(fake_with(&[(DEVELOPER, broken)])));
    let result = start_on(&pool, other, serde_json::json!({"brief": "x", "repo": "/workspace/app", "ref": "v2", "persona": "developer"}))
        .await
        .expect("started");
    let child = child_of(&pool, other, &result).await;
    eventually("the start fails", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_FAILED) }).await;
    let report: String = sqlx::query_scalar("SELECT body FROM agent_messages WHERE to_conversation_id = $1 AND kind = 'report'")
        .bind(other)
        .fetch_one(&pool)
        .await
        .expect("the report");
    assert!(report.contains("unknown field `tools`"), "the parse error is the report: {report}");
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(other) }).await;
}

/// `smelt.model` on another provider is the child's; one no provider has
/// keeps its parent's, and the parent is told.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_personas_model_is_resolved_against_the_providers(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let (first, _) = db::get_default_model(&pool).await.expect("read the default").expect("a default");
    let fake = Arc::new(fake_with(&[(DEVELOPER, persona_file(Some("big-model"), "Work.\n"))]));
    let root = root_with_checkout(&pool, Some(true), fake.clone()).await;
    // A second provider has `big-model`; the root stays on the first.
    mock_upstream(&pool, vec![text_reply("ok")]).await;
    let (second, _) = db::get_default_model(&pool).await.expect("read the default").expect("a default");
    db::set_conversation_model(&pool, root, first, crate::providers::test_support::MOCK_MODEL).await.expect("root model");
    db::ensure_provider_model(&pool, second, "big-model").await.expect("add the model");

    let input = serde_json::json!({"brief": "x", "repo": "/workspace/app", "ref": "v2", "persona": "developer"});
    let child = child_of(&pool, root, &start_on(&pool, root, input.clone()).await.expect("started")).await;
    eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
    let row = db::get_conversation_model(&pool, child).await.expect("model").expect("row");
    assert_eq!((row.provider_id, row.model.as_deref()), (Some(second), Some("big-model")));

    fake.files.lock().expect("files").insert(DEVELOPER.to_string(), persona_file(Some("no-such-model"), "Work.\n"));
    let other = child_of(&pool, root, &start_on(&pool, root, input).await.expect("started")).await;
    eventually("the child goes active", || async { state(&pool, other).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
    let row = db::get_conversation_model(&pool, other).await.expect("model").expect("row");
    assert_eq!((row.provider_id, row.model.as_deref()), (Some(first), Some(crate::providers::test_support::MOCK_MODEL)));
    let notices: Vec<String> = sqlx::query_scalar("SELECT body FROM agent_messages WHERE to_conversation_id = $1 AND kind = 'notice'")
        .bind(root)
        .fetch_all(&pool)
        .await
        .expect("notices");
    assert!(
        notices.contains(&format!(
            "agent #{other} (app:developer)'s persona asks for the model `no-such-model`, which none of the providers has; it runs on {} instead",
            crate::providers::test_support::MOCK_MODEL
        )),
        "{notices:?}"
    );
    eventually("every turn is over", || async {
        !crate::turn::turn_running(root) && !crate::turn::turn_running(child) && !crate::turn::turn_running(other)
    })
    .await;
}
