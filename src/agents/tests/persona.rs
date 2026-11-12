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
    assert_eq!(refused, "no persona `app:developer` in /workspace/app (its personas: app:reviewer); list_personas lists them");
    // A bare name with no repo: no checkout of the caller's records one.
    let refused = start_on(&pool, root, serde_json::json!({"brief": "x", "persona": "developer"})).await.expect_err("refused");
    assert_eq!(
        refused,
        "persona `developer` needs its repo: name it scoped, as list_personas does \
         (no checkout of yours has one; list_personas lists them)"
    );
    for bad in ["../x", "app:", "app:../x", "app:Developer"] {
        let refused = start_on(&pool, root, serde_json::json!({"brief": "x", "repo": "/workspace/app", "persona": bad}))
            .await
            .expect_err("refused");
        assert!(refused.contains("isn't a persona name"), "{bad}: {refused}");
    }
    assert_eq!(children(&pool, root).await, 0);
}

#[test]
fn test_split_name_splits_at_the_last_colon() {
    use crate::agents::persona::split_name;
    assert_eq!(split_name("app:developer"), (Some("app"), "developer"));
    assert_eq!(split_name("x:y:developer"), (Some("x:y"), "developer"));
    assert_eq!(split_name("developer"), (None, "developer"));
    assert_eq!(split_name("app:"), (Some("app"), ""));
}

const LIB_URL: &str = "https://example.com/team/lib.git";
const LIB_KEY: &str = "example.com/team/lib";

/// Records `files` as the persona files of `root`'s checkout `dir`, as a
/// clone or list_personas does.
async fn record_personas(pool: &PgPool, root: i64, dir: &str, files: &[&str]) {
    let files: Vec<String> = files.iter().map(|f| f.to_string()).collect();
    sqlx::query("UPDATE conversation_repos SET persona_files = $3 WHERE conversation_id = $1 AND dir = $2")
        .bind(root)
        .bind(dir)
        .bind(&files)
        .execute(pool)
        .await
        .expect("record persona files");
}

/// A scoped name names its repo: no `repo` needed, with or without a ref.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_scoped_persona_starts_with_no_repo(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let mut fake = fake_with(&[(DEVELOPER, persona_file(None, "Work.\n"))]);
    fake.parent = Some(Ok((COMMIT.to_string(), "main".to_string())));
    fake.parent_personas = vec![DEVELOPER.to_string()];
    let root = root_with_checkout(&pool, Some(true), Arc::new(fake)).await;
    let mut started = Vec::new();
    for input in [
        serde_json::json!({"brief": "x", "persona": "app:developer"}),
        serde_json::json!({"brief": "x", "persona": "app:developer", "ref": "v2"}),
        serde_json::json!({"brief": "x", "persona": "app:developer", "repo": "/workspace/app/"}),
    ] {
        let result = start_on(&pool, root, input.clone()).await.unwrap_or_else(|e| panic!("{input}: {e}"));
        assert!(result.contains("(app:developer)"), "{result}");
        let child = child_of(&pool, root, &result).await;
        eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
        let name: Option<String> = sqlx::query_scalar("SELECT persona_name FROM agents WHERE conversation_id = $1")
            .bind(child)
            .fetch_one(&pool)
            .await
            .expect("the child's row");
        assert_eq!(name.as_deref(), Some("developer"));
        let repos = db::list_conversation_repos(&pool, child).await.expect("repos");
        assert_eq!(repos.iter().map(|r| r.dir.as_str()).collect::<Vec<_>>(), ["app"]);
        let persona = db::agent_persona(&pool, child).await.expect("persona").expect("a persona");
        assert_eq!(persona.persona_path, "/workspace/app/.agents/personas/developer.md");
        started.push(child);
    }
    eventually("every turn is over", || async {
        !crate::turn::turn_running(root) && started.iter().all(|c| !crate::turn::turn_running(*c))
    })
    .await;
}

/// Two checkouts each with a `developer`: the scoped name picks its own,
/// and a bare name with no repo is refused, naming both.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_two_repos_developer_scoped_picks_its_repo_and_bare_is_refused(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let mut fake = fake_with(&[
        (format!("app/{DEVELOPER}").as_str(), persona_file(None, "App's work.\n")),
        (format!("lib/{DEVELOPER}").as_str(), persona_file(None, "Lib's work.\n")),
    ]);
    fake.parent = Some(Ok((COMMIT.to_string(), "main".to_string())));
    fake.parent_personas_in = [("app", vec![DEVELOPER.to_string()]), ("lib", vec![DEVELOPER.to_string()])]
        .into_iter()
        .map(|(dir, paths)| (dir.to_string(), paths))
        .collect();
    let root = root_with_checkout(&pool, Some(true), Arc::new(fake)).await;
    let lib = db::create_conversation_repo(&pool, root, LIB_URL, LIB_KEY, None, "lib").await.expect("a repo row");
    db::set_repo_cloned(&pool, lib.id, "main", Some(COMMIT)).await.expect("ready");
    db::set_repo_trust(&pool, LIB_KEY, true).await.expect("trust");
    record_personas(&pool, root, "app", &[DEVELOPER]).await;
    record_personas(&pool, root, "lib", &[DEVELOPER, ".agents/personas/reviewer.md"]).await;

    let result = start_on(&pool, root, serde_json::json!({"brief": "x", "persona": "lib:developer"})).await.expect("started");
    assert!(result.contains("(lib:developer)") && result.contains(LIB_URL), "{result}");
    let child = child_of(&pool, root, &result).await;
    eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
    let persona = db::agent_persona(&pool, child).await.expect("persona").expect("a persona");
    assert_eq!(persona.persona_path, "/workspace/lib/.agents/personas/developer.md");
    assert!(persona.persona_body.contains("Lib's work."), "{}", persona.persona_body);

    let refused = start_on(&pool, root, serde_json::json!({"brief": "x", "persona": "developer"})).await.expect_err("refused");
    assert_eq!(
        refused,
        "persona `developer` needs its repo: name it scoped, as list_personas does (app:developer, lib:developer)"
    );
    assert_eq!(children(&pool, root).await, 1, "a refusal writes nothing");
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_scoped_persona_and_a_different_repo_are_refused(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = root_with_checkout(&pool, Some(true), Arc::new(fake_with(&[]))).await;
    let refused = start_on(&pool, root, serde_json::json!({"brief": "x", "persona": "app:developer", "repo": "/workspace/lib"}))
        .await
        .expect_err("refused");
    assert_eq!(
        refused,
        "persona `app:developer` is in /workspace/app, but repo is /workspace/lib; drop repo or make them agree"
    );
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

/// P3 (SME-493): the snapshot carries the persona's `compact_at`, which
/// the turn reads from it; a file without one carries a null.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_snapshot_carries_the_personas_compact_at(pool: PgPool) {
    let with = "---\nname: developer\ndescription: d\nsmelt:\n  compact_at: 120000\n---\nbody\n".to_string();
    for (file, expected) in [(with, serde_json::json!(120000)), (persona_file(None, "body\n"), serde_json::Value::Null)] {
        let fake = Arc::new(fake_with(&[(DEVELOPER, file)]));
        let root = root_with_checkout(&pool, Some(true), fake.clone()).await;
        let repo = db::list_conversation_repos(&pool, root).await.expect("repos").into_iter().next().expect("a repo");
        let snapshot = crate::agents::persona::snapshot(&pool, fake.as_ref(), root, &repo, "developer")
            .await
            .expect("a snapshot");
        assert_eq!(snapshot.settings["compact_at"], expected, "{:?}", snapshot.settings);
    }
}

/// T8 (SME-506): a key the file lists in `smelt.ignore_if_unknown` and
/// this smelt doesn't know is skipped: the child starts, its parent is
/// told, the snapshot keeps the name, and a known restriction beside it
/// still reaches the rule chain.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_listed_unknown_key_is_ignored_and_its_parent_is_told(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let file = "---\nname: developer\ndescription: d\nsmelt:\n  tools:\n    deny: [write_file]\n  compact_later: 5\n  \
                ignore_if_unknown: [compact_later]\n---\nWork.\n"
        .to_string();
    let root = root_with_checkout(&pool, Some(true), Arc::new(fake_with(&[(DEVELOPER, file)]))).await;
    let input = serde_json::json!({"brief": "x", "repo": "/workspace/app", "ref": "v2", "persona": "developer"});
    let child = child_of(&pool, root, &start_on(&pool, root, input).await.expect("started")).await;
    eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;

    let settings: serde_json::Value = sqlx::query_scalar("SELECT persona_settings FROM agents WHERE conversation_id = $1")
        .bind(child)
        .fetch_one(&pool)
        .await
        .expect("the snapshot's settings");
    assert_eq!(settings["ignored"], serde_json::json!(["compact_later"]), "{settings}");
    let chain: Option<serde_json::Value> = sqlx::query_scalar("SELECT rule_chain FROM agents WHERE conversation_id = $1")
        .bind(child)
        .fetch_one(&pool)
        .await
        .expect("the chain column");
    assert!(chain.is_some_and(|c| c.to_string().contains("write_file")), "the deny beside the skipped key is kept");
    let notices: Vec<String> = sqlx::query_scalar("SELECT body FROM agent_messages WHERE to_conversation_id = $1 AND kind = 'notice'")
        .bind(root)
        .fetch_all(&pool)
        .await
        .expect("notices");
    assert!(
        notices.iter().any(|n| n.contains(&format!("agent #{child} (app:developer)'s persona at {}", &COMMIT[..7]))
            && n.contains("`smelt.compact_later`, which this smelt doesn't know")),
        "{notices:?}"
    );
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
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
            "start failed: no persona `app:developer` in https://example.com/team/app.git at {COMMIT}"
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
