use super::*;

/// Scenario 23 (SME-42): a dev server in the sandbox, end to end. It's
/// bound to 127.0.0.1 inside the pod, as dev servers are by default. The
/// model's own browser loads it at localhost; the model shares a preview;
/// the link shows up in the sandbox panel live, opens the same server in a
/// tab of its own, and survives a reload. Then (SME-33) the same for a
/// Docker container in the pod.
pub(super) async fn scenario_sandbox_dev_server(t: &Scenario<'_>) {
    let pool = t.pool;
    let serving = t.conversation().await;
    sandbox::create_pod(pool, serving.id, Default::default()).await.expect("create_pod");
    let terminal = sandbox::create_terminal(pool, serving.id).await.expect("create_terminal");
    anthropic::tools::execute(
        pool,
        serving.id,
        &unique_id("write"),
        "write_file",
        &serde_json::json!({
            "path": "/home/sandbox/site/index.html",
            "content": "<html><body><h1>Hello from the sandbox dev server</h1></body></html>",
        }),
    )
    .await
    .expect("write the page");
    anthropic::tools::execute(
        pool,
        serving.id,
        &unique_id("serve"),
        "run_terminal_command",
        &serde_json::json!({
            "terminal_id": terminal,
            "command": "cd /home/sandbox/site && python3 -m http.server 8000 --bind 127.0.0.1",
        }),
    )
    .await
    .expect("start the server");
    let mut listening = false;
    for _ in 0..50 {
        if sandbox::pod_port_is_listening(pool, serving.id, sandbox::PodHost::Localhost, 8000)
            .await
            .expect("probe port 8000")
        {
            listening = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(listening, "the dev server never started listening");

    scenario_sandbox_dev_server_models_side(pool, &serving).await;
    scenario_sandbox_dev_server_users_side(t, pool, &serving).await;
}


/// The model's side: localhost in its browsing session is the sandbox's own.
async fn scenario_sandbox_dev_server_models_side(pool: &sqlx::PgPool, serving: &crate::models::Conversation) {
    // The model's side: localhost in its browsing session is the sandbox.
    crate::browsing::open_session(serving.id, Some(crate::egress_proxy::sandbox_dial(pool.clone(), serving.id)))
        .await
        .expect("open a browsing session");
    let seen = crate::browsing::navigate(serving.id, "http://localhost:8000/").await;
    crate::browsing::close_session(serving.id).await.expect("close the browsing session");
    let seen = seen.expect("the model's browser should load the sandbox's dev server");
    assert!(seen.text.contains("Hello from the sandbox dev server"), "got {:?}", seen.text);
}

/// The user's side: a preview link, live and after a reload; then a Docker container's server, for the model and as a preview.
async fn scenario_sandbox_dev_server_users_side(t: &Scenario<'_>, pool: &sqlx::PgPool, serving: &crate::models::Conversation) {
    // The user's side.
    let page = t.tab(t.url(&format!("conversation/{}", serving.id))).await;
    wait_for_live_client(&page, serving.id).await;
    let shared = anthropic::tools::execute(
        pool,
        serving.id,
        &unique_id("preview"),
        "sandbox_preview_url",
        &serde_json::json!({"port": 8000}),
    )
    .await
    .expect("share a preview");
    let shared: serde_json::Value = serde_json::from_str(&shared).expect("the tool's JSON");
    assert_eq!(shared["listening"], true, "got {shared}");
    let link = shared["url"].as_str().expect("a url").to_string();
    assert_eq!(link, crate::preview::configured_template().expect("the template").url_for(serving.id, crate::sandbox::PodHost::Localhost, 8000));
    // Without a reload: it arrives on the conversation's live stream.
    wait_for_element(&page, ".sandbox-preview", Duration::from_secs(10)).await;
    let href: String = page
        .evaluate("document.querySelector('.sandbox-preview').href")
        .await
        .expect("read the link")
        .into_value()
        .expect("an href");
    assert_eq!(href.trim_end_matches('/'), link, "the panel links to the shared preview");
    let preview_tab = t.tab(link.clone()).await;
    assert!(
        wait_for_text(&preview_tab, "Hello from the sandbox dev server", Duration::from_secs(10)).await,
        "the preview link should show the sandbox's dev server"
    );
    preview_tab.close().await.expect("close the preview tab");
    page.reload().await.expect("reload the conversation");
    wait_for_live_client(&page, serving.id).await;
    // Still there after a reload: `get_sandbox_state` brings it back.
    wait_for_element(&page, ".sandbox-preview", Duration::from_secs(10)).await;

    // SME-33: a Docker container in the pod, its port not published, as
    // both browsers reach it on a Linux host: at its own address. A
    // network with a fixed address saves reading it from the terminal;
    // the base image is the sandbox's own files, so no Docker Hub.
    let container = std::net::Ipv4Addr::new(172, 21, 9, 9);
    anthropic::tools::execute(
        pool,
        serving.id,
        &unique_id("write"),
        "write_file",
        &serde_json::json!({
            "path": "/workspace/site/index.html",
            "content": "<html><body><h1>Hello from a Docker container</h1></body></html>",
        }),
    )
    .await
    .expect("write the container's page");
    // SME-90: Chrome sends a container's plain-http address no
    // Sec-Fetch-*, so the egress proxy goes by Referer. The page's own
    // script carries one and loads; one fetched with no referrer arrives
    // with nothing to tell it from another site's, and is refused.
    for (path, content) in [
        (
            "/workspace/site/scripts.html",
            "<html><body><p id=out>scripts:</p>\
             <script src=\"own.js\" onload=\"out.append(' own-loaded')\" onerror=\"out.append(' own-refused')\"></script>\
             <script src=\"anon.js\" referrerpolicy=\"no-referrer\" onload=\"out.append(' anon-loaded')\" \
             onerror=\"out.append(' anon-refused')\"></script></body></html>",
        ),
        ("/workspace/site/own.js", "1;"),
        ("/workspace/site/anon.js", "1;"),
    ] {
        anthropic::tools::execute(
            pool,
            serving.id,
            &unique_id("write"),
            "write_file",
            &serde_json::json!({"path": path, "content": content}),
        )
        .await
        .expect("write the container's script page");
    }
    let docker_terminal = sandbox::create_terminal(pool, serving.id).await.expect("create_terminal");
    anthropic::tools::execute(
        pool,
        serving.id,
        &unique_id("container"),
        "run_terminal_command",
        &serde_json::json!({
            "terminal_id": docker_terminal,
            "command": format!(
                "sudo tar -C / -c bin sbin lib lib64 usr etc 2>/dev/null | docker import - local/base \
                 && docker network create --subnet 172.21.9.0/24 site \
                 && docker run -d --network site --ip {container} -v /workspace/site:/w -w /w \
                    local/base python3 -m http.server 8001"
            ),
        }),
    )
    .await
    .expect("start the container");
    let host = sandbox::PodHost::Container(container);
    let mut listening = false;
    for _ in 0..300 {
        if sandbox::pod_port_is_listening(pool, serving.id, host, 8001).await.expect("probe the container") {
            listening = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(listening, "the container's server never started listening");

    // The model's side: webfetch and a browsing session.
    let fetched = anthropic::tools::execute(
        pool,
        serving.id,
        &unique_id("fetch"),
        "webfetch",
        &serde_json::json!({"url": format!("http://{container}:8001/")}),
    )
    .await
    .expect("webfetch the container");
    assert!(fetched.contains("Hello from a Docker container"), "got {fetched}");
    crate::browsing::open_session(serving.id, Some(crate::egress_proxy::sandbox_dial(pool.clone(), serving.id)))
        .await
        .expect("open a browsing session");
    let seen = crate::browsing::navigate(serving.id, &format!("http://{container}:8001/")).await;
    let scripts = crate::browsing::navigate(serving.id, &format!("http://{container}:8001/scripts.html")).await;
    crate::browsing::close_session(serving.id).await.expect("close the browsing session");
    let seen = seen.expect("the model's browser should load the container's server");
    assert!(seen.text.contains("Hello from a Docker container"), "got {:?}", seen.text);
    let scripts = scripts.expect("the model's browser should load the container's script page");
    assert!(
        scripts.text.contains("own-loaded") && scripts.text.contains("anon-refused"),
        "the page's own script should load and an unidentified one be refused: {:?}",
        scripts.text
    );

    // The user's side: a preview of the container, in the panel and in a tab.
    let shared = anthropic::tools::execute(
        pool,
        serving.id,
        &unique_id("preview"),
        "sandbox_preview_url",
        &serde_json::json!({"port": 8001, "host": container.to_string()}),
    )
    .await
    .expect("share the container's preview");
    let shared: serde_json::Value = serde_json::from_str(&shared).expect("the tool's JSON");
    assert_eq!(shared["listening"], true, "got {shared}");
    let link = shared["url"].as_str().expect("a url").to_string();
    assert!(wait_for_text(&page, "172.21.9.9:8001", Duration::from_secs(10)).await, "the panel should name the container");
    let preview_tab = t.tab(link).await;
    assert!(
        wait_for_text(&preview_tab, "Hello from a Docker container", Duration::from_secs(10)).await,
        "the container's preview link should show its server"
    );
}

/// A repo's AGENTS.md waits for the user's trust (SME-32): when the model
/// asks to load one from an unknown remote, the chat shows the file with
/// Trust / Don't trust, and trusting loads exactly that file. The model the
/// decision wakes is the mock upstream.
pub(super) async fn scenario_agents_md_trust(t: &Scenario<'_>) {
    let pool = t.pool;
    let trusting = t.conversation().await;
    let remote = format!("example.com/browser-tier/{}", unique_id("trust"));
    let repo = db::create_conversation_repo(pool, trusting.id, &format!("https://{remote}.git"), &remote, None, "trust-me")
        .await
        .expect("seed a repo");
    db::set_repo_cloned(pool, repo.id, "main", Some("abc1234def")).await.expect("seed the clone");
    // The model asked to load its AGENTS.md (load_instructions).
    db::request_instruction(
        pool,
        trusting.id,
        repo.id,
        "AGENTS.md",
        &db::InstructionsFile {
            content: "Browser tier rule: always run the linter.\n".to_string(),
            file_bytes: 42,
            hash: "browser-tier-hash".to_string(),
            commit: Some("abc1234def".to_string()),
        },
    )
    .await
    .expect("seed the request");
    let trust_page = t.tab(t.url(&format!("conversation/{}", trusting.id))).await;
    assert!(
        wait_for_text(&trust_page, "Browser tier rule: always run the linter.", Duration::from_secs(10)).await,
        "the trust card should show the AGENTS.md it asks about"
    );
    // The repo shows in the panel with no pod running (SME-32 code
    // review 10, finding 1).
    // (By the panel's own element: the card's file path has the same text.)
    assert!(
        wait_for_count(&trust_page, ".sandbox-repo", 1, Duration::from_secs(10)).await,
        "the panel should list the conversation's repos even without a pod"
    );
    click_when_present(&trust_page, ".trust-card-trust", Duration::from_secs(5)).await;
    assert!(
        wait_for_count(&trust_page, ".trust-card", 0, Duration::from_secs(10)).await,
        "trusting should take the card away, live"
    );
    let trusted = db::get_repo_trust(pool, &remote).await.expect("read trust");
    db::delete_repo_trust(pool, &remote).await.expect("clean up the trust decision");
    assert_eq!(trusted, Some(true), "the decision is remembered for the remote");
    assert_eq!(
        crate::git::project_instructions(pool, trusting.id).await.expect("loaded")[0].content,
        "Browser tier rule: always run the linter.\n"
    );

    // SME-277: an AGENTS.md that hides text, through the real flow rather
    // than a seed — the model clones a repo whose file hides characters and
    // asks to load it, so the card's row comes from `clone_repo` and
    // `load_instructions` themselves. The card then shows the escapes and
    // the count in the live DOM, and Trust still loads the file exactly as
    // it is, invisible characters and all.
    let hiding = t.conversation().await;
    sandbox::create_pod(pool, hiding.id, Default::default()).await.expect("create_pod");
    let terminal = sandbox::create_terminal(pool, hiding.id).await.expect("create_terminal");
    // Python's own escapes, so the shell command carries only ASCII: three
    // zero-width-ish characters, a bidi isolate pair around "re order" (a
    // reader sees "re order" in the wrong order without the fix) and a BOM.
    let make_hiding_repo = r##"git init --bare --initial-branch=main -q /tmp/v.git && git clone -q file:///tmp/v.git /tmp/w && cd /tmp/w && python3 -c 'open("AGENTS.md","w").write("# House rules\nBe kind.\u200b\u206e\u2066re order\u200b\nRead every file.\ufeff\n")' && git add AGENTS.md && git -c user.email=smelt@test -c user.name=smelt commit -qm rules && git push -q origin main"##;
    anthropic::tools::execute(
        pool,
        hiding.id,
        &unique_id("make"),
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal, "command": make_hiding_repo}),
    )
    .await
    .expect("make the hiding repo");
    // `run_terminal_command` only *starts* those git commands: the tier has
    // to wait for the finish before the clone, or it races — a half-made
    // origin clones empty, and the load says "there's no file". The second
    // run of this scenario proved it.
    wait_command_finished(pool, terminal).await;
    anthropic::tools::execute(
        pool,
        hiding.id,
        &unique_id("clone"),
        "clone_repo",
        &serde_json::json!({"url": "file:///tmp/v.git"}),
    )
    .await
    .expect("clone the hiding repo");
    anthropic::tools::execute(
        pool,
        hiding.id,
        &unique_id("load"),
        "load_instructions",
        &serde_json::json!({"path": "/workspace/v/AGENTS.md"}),
    )
    .await
    .expect("ask to load the hiding AGENTS.md");
    let hidden_page = t.tab(t.url(&format!("conversation/{}", hiding.id))).await;
    assert!(
        wait_for_text(&hidden_page, "\\u{200b}", Duration::from_secs(10)).await,
        "the card should show the zero-width space escaped as \\u{{…}} text"
    );
    assert!(
        wait_for_text(&hidden_page, "\\u{feff}", Duration::from_secs(10)).await,
        "the card should show the BOM escaped too"
    );
    assert!(
        wait_for_text(&hidden_page, "5 characters here render as nothing", Duration::from_secs(10)).await,
        "the card should name the count of characters it had to escape"
    );
    click_when_present(&hidden_page, ".trust-card-trust", Duration::from_secs(5)).await;
    assert!(
        wait_for_count(&hidden_page, ".trust-card", 0, Duration::from_secs(10)).await,
        "trusting should take the hidden-characters card away too, live"
    );
    assert_eq!(
        crate::git::project_instructions(pool, hiding.id).await.expect("loaded")[0].content,
        "# House rules\nBe kind.\u{200b}\u{206e}\u{2066}re order\u{200b}\nRead every file.\u{feff}\n",
        "Trust loads the raw file, not the escaped showing"
    );
}

/// SME-279: a card about the repo itself — what a refused `start_agent`
/// asks with, on a repo that has no AGENTS.md to ask with. The row is
/// seeded with the same helper that refusal uses
/// (`db::request_repo_trust_card`); the refusal itself is
/// `agents::tests::repo`'s and the decision is `git::server::db_tests`'s.
/// What this scenario holds is the card's own wire and click path: the
/// `is_repo` flag reaching the hydrated card, the checkout path it quotes
/// instead of a file, no preview, and the empty-string hash the buttons
/// send back deciding rather than erroring — with nothing loaded after.
pub(super) async fn scenario_repo_trust_card(t: &Scenario<'_>) {
    let pool = t.pool;
    let asking = t.conversation().await;
    let remote = format!("example.com/browser-tier/{}", unique_id("repo-card"));
    let repo = db::create_conversation_repo(
        pool,
        asking.id,
        &format!("https://{remote}.git"),
        &remote,
        None,
        "repo-card",
    )
    .await
    .expect("seed a repo");
    db::set_repo_cloned(pool, repo.id, "main", Some("abc1234def")).await.expect("seed the clone");
    db::request_repo_trust_card(pool, asking.id, repo.id).await.expect("seed the repo card");

    let page = t.tab(t.url(&format!("conversation/{}", asking.id))).await;
    assert!(
        wait_for_text(&page, "wants to start an agent on this repo", Duration::from_secs(10)).await,
        "the repo card should say what a refused start_agent is asking about"
    );
    assert!(
        wait_for_text(&page, "work on /workspace/repo-card, and", Duration::from_secs(5)).await,
        "the repo card should quote the checkout itself, not a file under it"
    );
    assert!(
        wait_for_count(&page, ".trust-card", 1, Duration::from_secs(5)).await,
        "the conversation should show exactly the one repo card"
    );
    assert!(
        wait_for_count(&page, ".trust-card-preview", 0, Duration::from_secs(5)).await,
        "a repo card has no file preview to show"
    );
    assert!(
        !wait_for_text(&page, "load this AGENTS.md", Duration::from_secs(2)).await,
        "a repo card is not a file card"
    );

    click_when_present(&page, ".trust-card-trust", Duration::from_secs(5)).await;
    assert!(
        wait_for_count(&page, ".trust-card", 0, Duration::from_secs(10)).await,
        "trusting should take the card away, live"
    );
    let trusted = db::get_repo_trust(pool, &remote).await.expect("read trust");
    assert_eq!(trusted, Some(true), "the decision is remembered for the remote");
    assert!(
        crate::git::project_instructions(pool, asking.id).await.expect("loaded").is_empty(),
        "trusting the repo card loads no file"
    );
    db::delete_repo_trust(pool, &remote).await.expect("clean up the trust decision");
}

/// SME-34: the model's question waits on a card. It shows in every tab and
/// survives a reload, the sidebar marks the conversation, and choosing an
/// option answers it: the card goes from every tab, the mark clears, and
/// the call becomes a row with the answer the model got.
pub(super) async fn scenario_ask_user_card(t: &Scenario<'_>) {
    let pool = t.pool;
    let asking = t.conversation().await;
    seed_user_message(pool, asking.id, "clean up the build").await;
    let input = serde_json::json!({"questions": [
        {"question": "Delete the build directory?", "header": "Delete",
         "options": [{"label": "Yes (Recommended)"}, {"label": "No", "description": "keep it"}]}
    ]});
    let tool_use_id = unique_id("toolu_ask");
    db::create_message(
        pool,
        asking.id,
        "assistant",
        &[anthropic::ContentBlock::ToolUse { id: tool_use_id.clone(), name: crate::questions::ASK_USER.to_string(), input: input.clone() }],
    )
    .await
    .expect("seed the call");
    let questions = crate::questions::parse_questions(&input).expect("valid questions");
    db::create_pending_question(pool, asking.id, &tool_use_id, &questions).await.expect("seed the question");

    let url = t.url(&format!("conversation/{}", asking.id));
    let first = t.tab(url.clone()).await;
    let second = t.tab(url.clone()).await;
    for page in [&first, &second] {
        assert!(
            wait_for_text(page, "Delete the build directory?", Duration::from_secs(10)).await,
            "every tab shows the question"
        );
        assert!(wait_for_count(page, ".question-option", 2, Duration::from_secs(5)).await, "with its two options");
    }
    let mark = format!("[data-conversation-id='{}'] .conversation-waiting", asking.id);
    assert!(wait_for_count(&first, &mark, 1, Duration::from_secs(10)).await, "the sidebar marks it waiting");

    first.goto(url.as_str()).await.expect("reload");
    assert!(
        wait_for_count(&first, ".question-card", 1, Duration::from_secs(10)).await,
        "the card survives a reload"
    );

    click_when_present(&first, ".question-option", Duration::from_secs(5)).await;
    for page in [&first, &second] {
        assert!(
            wait_for_count(page, ".question-card", 0, Duration::from_secs(10)).await,
            "answering takes the card away from every tab"
        );
        assert!(
            wait_for_text(page, "1. Delete: Yes (Recommended)", Duration::from_secs(10)).await,
            "the call shows the answer the model got"
        );
    }
    assert!(wait_for_count(&first, &mark, 0, Duration::from_secs(10)).await, "the mark clears");
}
