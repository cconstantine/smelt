use super::*;

#[test]
fn test_chat_error_text_drops_the_server_function_wrapper() {
    let error = ServerFnError::new("model provider error 503 Service Unavailable: paused");
    assert_eq!(
        chat_error_text(&error),
        "model provider error 503 Service Unavailable: paused"
    );
}

fn prompt_env() -> PromptEnvironment {
    PromptEnvironment {
        date: chrono::NaiveDate::from_ymd_opt(2026, 9, 25).expect("a valid date"),
        model: "claude-test-model".to_string(),
        volumes: vec![("cargo-cache".to_string(), "/home/sandbox/.cargo".to_string())],
        mcp_servers: vec!["exa".to_string(), "github".to_string()],
        repos: Vec::new(),
        instructions: Vec::new(),
        persona_files: Default::default(),
    }
}

fn ready_repo(id: i64, dir: &str, agents_files: Vec<String>) -> crate::git::RepoSummary {
    crate::git::RepoSummary {
        id,
        url: format!("git@github.com:o/{dir}.git"),
        path: format!("/workspace/{dir}"),
        requested_branch: None,
        branch: Some("main".to_string()),
        commit: Some("43835b44f939".to_string()),
        status: crate::git::RepoStatus::Ready,
        error: None,
        agents_files,
        loaded_instructions: vec![],
        trust_requests: vec![],
    }
}

/// SME-235: each repo's line names its persona files, like its AGENTS.md
/// files: names only, so for a repo the user hasn't trusted too.
#[test]
fn test_system_prompt_names_each_repos_persona_files() {
    let mut env = prompt_env();
    env.repos = vec![
        ready_repo(1, "smelt", vec!["AGENTS.md".to_string()]),
        ready_repo(2, "bare", vec![]),
        ready_repo(3, "plain", vec![]),
    ];
    env.persona_files = [
        (1, vec![".agents/personas/developer.md".to_string(), ".agents/personas/reviewer.md".to_string()]),
        (2, vec![".agents/personas/developer.md".to_string()]),
        (3, vec![]),
    ]
    .into_iter()
    .collect();
    let prompt = system_prompt(&env);
    assert!(
        prompt.contains(
            "  - /workspace/smelt: git@github.com:o/smelt.git (main); AGENTS.md files: AGENTS.md; \
             persona files: .agents/personas/developer.md, .agents/personas/reviewer.md\n"
        ),
        "{prompt}"
    );
    assert!(
        prompt.contains("  - /workspace/bare: git@github.com:o/bare.git (main); persona files: .agents/personas/developer.md\n"),
        "{prompt}"
    );
    assert!(prompt.contains("  - /workspace/plain: git@github.com:o/plain.git (main)\n"), "{prompt}");
}

#[test]
fn test_system_prompt_lists_repos_and_ends_with_their_instructions() {
    let mut env = prompt_env();
    env.repos = vec![crate::git::RepoSummary {
        id: 1,
        url: "git@github.com:o/smelt.git".to_string(),
        path: "/workspace/smelt".to_string(),
        requested_branch: None,
        branch: Some("main".to_string()),
        commit: Some("43835b44f939".to_string()),
        status: crate::git::RepoStatus::Ready,
        error: None,
        agents_files: vec!["AGENTS.md".to_string(), "web/AGENTS.md".to_string()],
        loaded_instructions: vec!["AGENTS.md".to_string()],
        trust_requests: vec![],
    }];
    env.instructions = vec![crate::git::ProjectInstructions {
        repo_url: "git@github.com:o/smelt.git".to_string(),
        path: "/workspace/smelt/AGENTS.md".to_string(),
        commit: Some("43835b44f939".to_string()),
        content: "Run make test.\n".to_string(),
        file_bytes: 15,
        truncated: false,
    }];
    let prompt = system_prompt(&env);
    assert!(
        prompt.contains("- Repositories:\n  - /workspace/smelt: git@github.com:o/smelt.git (main); AGENTS.md files: AGENTS.md (loaded), web/AGENTS.md\n"),
        "{prompt}"
    );
    assert!(
        prompt.ends_with(&crate::git::render_project_instructions(&env.instructions)),
        "{prompt}"
    );
}

/// SME-30: replies render as markdown now, so the prompt stops telling
/// the model to avoid it.
#[test]
fn test_system_prompt_says_markdown_renders() {
    assert!(!BASE_SYSTEM_PROMPT.contains("Markdown is not rendered"), "the prompt still says markdown doesn't render");
    assert!(!BASE_SYSTEM_PROMPT.contains("Don't use tables, headings"), "the prompt still rules out tables and headings");
    assert!(BASE_SYSTEM_PROMPT.contains("rendered as Markdown"), "the prompt should say replies are rendered as Markdown");
}

#[test]
fn test_system_prompt_starts_with_the_base_prompt() {
    let prompt = system_prompt(&prompt_env());
    assert!(prompt.starts_with(BASE_SYSTEM_PROMPT), "got: {prompt}");
}

#[test]
fn test_system_prompt_describes_the_environment() {
    let prompt = system_prompt(&prompt_env());
    let environment = prompt
        .strip_prefix(BASE_SYSTEM_PROMPT)
        .expect("the environment follows the base prompt");
    for expected in [
        "Today's date: 2026-09-25",
        "Model: claude-test-model",
        "cargo-cache mounted at /home/sandbox/.cargo",
        "MCP servers: exa, github",
    ] {
        assert!(environment.contains(expected), "missing {expected:?} in: {environment}");
    }
}

#[test]
fn test_system_prompt_leaves_out_empty_volume_and_mcp_lines() {
    let mut env = prompt_env();
    env.volumes.clear();
    env.mcp_servers.clear();
    let prompt = system_prompt(&env);
    let environment = prompt
        .strip_prefix(BASE_SYSTEM_PROMPT)
        .expect("the environment follows the base prompt");
    assert!(environment.contains("Today's date: 2026-09-25"), "got: {environment}");
    assert!(!environment.contains("mounted at"), "got: {environment}");
    assert!(!environment.contains("MCP servers"), "got: {environment}");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_prompt_environment_reads_volumes_and_mcp_servers(pool: PgPool) {
    db::create_sandbox_volume(&pool, "cargo-cache", "/home/sandbox/.cargo")
        .await
        .expect("create volume");
    db::ensure_mcp_server(&pool, "exa", "https://mcp.example.com/mcp")
        .await
        .expect("add mcp server");
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let repo = db::create_conversation_repo(&pool, conversation.id, "git@github.com:o/r.git", "github.com/o/r", None, "r")
        .await
        .expect("repo");
    db::set_repo_cloned(&pool, repo.id, "main", Some("abc123")).await.expect("cloned");
    db::set_repo_agents_files(&pool, repo.id, &["AGENTS.md".to_string(), "web/AGENTS.md".to_string()])
        .await
        .expect("agents files");
    let loaded = db::InstructionsFile {
        content: "Run make test.\n".to_string(),
        file_bytes: 15,
        hash: "h".to_string(),
        commit: Some("abc123".to_string()),
    };
    db::load_instruction(&pool, conversation.id, repo.id, "AGENTS.md", &loaded).await.expect("load");
    sqlx::query("UPDATE conversation_repos SET persona_files = '{.agents/personas/developer.md}' WHERE id = $1")
        .bind(repo.id)
        .execute(&pool)
        .await
        .expect("persona files");
    let env = prompt_environment(&pool, conversation.id, "some-model").await;
    assert_eq!(
        env.persona_files,
        [(repo.id, vec![".agents/personas/developer.md".to_string()])].into_iter().collect()
    );
    assert_eq!(env.repos[0].loaded_instructions, vec!["AGENTS.md".to_string()]);
    assert_eq!(env.repos.len(), 1);
    assert_eq!(env.repos[0].path, "/workspace/r");
    assert_eq!(
        env.instructions,
        vec![crate::git::ProjectInstructions {
            repo_url: "git@github.com:o/r.git".to_string(),
            path: "/workspace/r/AGENTS.md".to_string(),
            commit: Some("abc123".to_string()),
            content: "Run make test.\n".to_string(),
            file_bytes: 15,
            truncated: false,
        }]
    );
    assert_eq!(env.date, chrono::Utc::now().date_naive());
    assert_eq!(env.model, "some-model");
    assert_eq!(
        env.volumes,
        vec![("cargo-cache".to_string(), "/home/sandbox/.cargo".to_string())]
    );
    assert_eq!(env.mcp_servers, vec!["exa".to_string()]);
}

/// Every turn's request carries the system prompt built from that
/// turn's environment.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_run_turn_sends_the_system_prompt(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    db::create_sandbox_volume(&pool, "cargo-cache", "/home/sandbox/.cargo")
        .await
        .expect("create volume");
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;

    run_turn(&pool, conversation.id, hello())
        .await
        .expect("run_turn should succeed");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 1);
    let expected = system_prompt(
        &prompt_environment(&pool, conversation.id, crate::providers::test_support::MOCK_MODEL).await,
    );
    assert_eq!(requests[0]["system"].as_str(), Some(expected.as_str()));
    assert!(expected.contains("cargo-cache mounted at /home/sandbox/.cargo"));
}

/// Compaction's summarization call keeps its own prompt; only the real
/// turn after it gets the agent's system prompt.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_compaction_keeps_its_own_system_prompt(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    db::create_message(
        &pool,
        conversation.id,
        "user",
        &[anthropic::ContentBlock::Text {
            text: "earlier message".to_string(),
        }],
    )
    .await
    .expect("seed earlier message");
    // Usage past the ceiling, so the next turn compacts first (see
    // `test_run_turn_compacts_before_sending_when_usage_is_near_the_ceiling`).
    db::upsert_conversation_usage(
        &pool,
        conversation.id,
        &anthropic::TokenUsage {
            input_tokens: 190_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    )
    .await
    .expect("seed usage");
    let requests = start_recording_mock_upstream(&pool, vec![
        text_reply_body("Summary: nothing live."),
        text_reply_body("Hi again"),
    ])
    .await;

    run_turn(&pool, conversation.id, hello())
        .await
        .expect("run_turn should succeed");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 2, "a compaction call, then the real turn");
    assert_eq!(requests[0]["system"].as_str(), Some(COMPACTION_SYSTEM_PROMPT));
    let expected = system_prompt(
        &prompt_environment(&pool, conversation.id, crate::providers::test_support::MOCK_MODEL).await,
    );
    assert_eq!(requests[1]["system"].as_str(), Some(expected.as_str()));
}

/// On a provider with prompt caching on, the turn's request carries both
/// cache markers; compaction's one-off summarization call carries neither.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_caching_provider_marks_turn_requests_but_not_compaction(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    db::create_message(
        &pool,
        conversation.id,
        "user",
        &[anthropic::ContentBlock::Text {
            text: "earlier message".to_string(),
        }],
    )
    .await
    .expect("seed earlier message");
    db::upsert_conversation_usage(
        &pool,
        conversation.id,
        &anthropic::TokenUsage {
            input_tokens: 190_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    )
    .await
    .expect("seed usage");
    let requests = start_recording_mock_upstream(&pool, vec![
        text_reply_body("Summary: nothing live."),
        text_reply_body("Hi again"),
    ])
    .await;
    sqlx::query("UPDATE inference_providers SET prompt_caching = true")
        .execute(&pool)
        .await
        .expect("turn caching on");

    run_turn(&pool, conversation.id, hello())
        .await
        .expect("run_turn should succeed");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 2, "a compaction call, then the real turn");
    assert!(requests[0].get("cache_control").is_none(), "compaction isn't marked: {}", requests[0]);
    assert_eq!(requests[0]["system"].as_str(), Some(COMPACTION_SYSTEM_PROMPT));
    let ephemeral = serde_json::json!({"type": "ephemeral"});
    assert_eq!(requests[1]["cache_control"], ephemeral);
    let expected = system_prompt(
        &prompt_environment(&pool, conversation.id, crate::providers::test_support::MOCK_MODEL).await,
    );
    assert_eq!(
        requests[1]["system"],
        serde_json::json!([{"type": "text", "text": expected, "cache_control": ephemeral}])
    );
}

/// The detail view shows exactly the system prompt a turn sends.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_context_detail_shows_the_system_prompt_a_turn_sends(pool: PgPool) {
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let detail = context_detail(&pool, conversation.id)
        .await
        .expect("context detail");
    // No provider, so no model to name.
    let expected = system_prompt(&prompt_environment(&pool, conversation.id, "").await);
    assert_eq!(detail.system.as_deref(), Some(expected.as_str()));
}

/// The detail view lists the loaded AGENTS.md files on their own.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_context_detail_lists_loaded_instructions(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let repo = db::create_conversation_repo(&pool, conversation.id, "git@github.com:o/r.git", "github.com/o/r", None, "r")
        .await
        .expect("repo");
    let loaded = db::InstructionsFile {
        content: "Run make test.\n".to_string(),
        file_bytes: 15,
        hash: "h".to_string(),
        commit: Some("abc123".to_string()),
    };
    db::load_instruction(&pool, conversation.id, repo.id, "AGENTS.md", &loaded).await.expect("load");
    let detail = context_detail(&pool, conversation.id).await.expect("context detail");
    assert_eq!(detail.instructions.len(), 1);
    assert_eq!(detail.instructions[0].path, "/workspace/r/AGENTS.md");
    assert_eq!(detail.instructions[0].content, "Run make test.\n");
}

/// Every tool the base prompt names in backticks must exist, so
/// renaming or removing a tool fails here until the prompt is updated.
/// A backticked word counts as a tool name when it's lowercase letters
/// and underscores; the few that aren't tools are listed.
#[test]
fn test_system_prompt_only_names_real_tools() {
    // Backticked words that aren't tools: a user, commands, tool
    // parameters and an example container name.
    const NOT_TOOLS: &[&str] = &[
        "sandbox",
        "sudo",
        "web_search",
        "localhost",
        "docker_memory_limit",
        "host",
        "web",
    ];
    let tools: Vec<String> = anthropic::tools::native_tool_definitions()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    let named: Vec<&str> = BASE_SYSTEM_PROMPT
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|word| !word.is_empty() && word.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        .filter(|word| !NOT_TOOLS.contains(word))
        .collect();
    assert!(named.len() > 10, "expected the prompt to name its tools, found {named:?}");
    let unknown: Vec<&&str> = named.iter().filter(|word| !tools.iter().any(|t| t == **word)).collect();
    assert!(unknown.is_empty(), "the system prompt names tools that don't exist: {unknown:?}");
}
