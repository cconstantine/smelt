//! SME-235: a clone records its repo's persona files, and `list_personas`
//! reads them from the checkout, for a trusted repo only.

use super::*;

/// Runs `script` in pod `pod`'s sandbox container, and asserts it worked.
async fn sh(client: &kube::Client, pod: i64, script: &str) {
    let ran = exec_with(client, &pod_name(pod), "sandbox", &["sh", "-c", script], None)
        .await
        .expect("exec");
    assert_eq!(ran.exit_code, 0, "{script}: {}{}", ran.stdout, ran.stderr);
}

/// Makes a bare origin at `origin` with an AGENTS.md and, in
/// `.agents/personas/`: a good `developer.md`, a `broken.md` with Claude
/// Code's `tools` key, `nested/x.md` (not a persona: not directly in the
/// directory) and `link.md`, a symlink out of the checkout.
async fn make_origin_with_personas(client: &kube::Client, pod: i64, origin: &str) {
    let script = format!(
        "set -e; rm -rf /tmp/src; git init -q -b main /tmp/src; cd /tmp/src; mkdir -p .agents/personas/nested; \
         echo 'Run make test.' > AGENTS.md; \
         printf -- '---\\nname: developer\\ndescription: Works one ticket.\\nsmelt:\\n  model: claude-opus-4-5\\n---\\nRead the docs.\\n' > .agents/personas/developer.md; \
         printf -- '---\\nname: broken\\ndescription: d\\ntools: Read\\n---\\n' > .agents/personas/broken.md; \
         printf -- '---\\nname: x\\ndescription: d\\n---\\n' > .agents/personas/nested/x.md; \
         ln -s /etc/hostname .agents/personas/link.md; \
         git add -A; git -c user.name=t -c user.email=t@example.com commit -qm one; git clone -q --bare /tmp/src {origin}"
    );
    sh(client, pod, &script).await;
}

async fn persona_files(pool: &PgPool, conversation_id: i64, dir: &str) -> Vec<String> {
    let repos = db::list_conversation_repos(pool, conversation_id).await.expect("list repos");
    repos.into_iter().find(|r| r.dir == dir).expect("the repo").persona_files
}

fn personas_of(listing: &serde_json::Value, path: &str) -> Vec<serde_json::Value> {
    listing["repos"]
        .as_array()
        .expect("repos")
        .iter()
        .find(|r| r["path"] == path)
        .and_then(|r| r["personas"].as_array().cloned())
        .unwrap_or_else(|| panic!("no personas for {path}: {listing}"))
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_clone_records_persona_files_and_list_personas_shows_them_once_trusted(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let id = conversation.id;
        let pod = create_pod(&pool, id, PodLimitOverrides::default()).await.expect("create_pod");
        make_origin_with_personas(&client, pod, "/tmp/personas.git").await;
        for dir in [None, Some("again")] {
            crate::git::clone_repo(&pool, id, "file:///tmp/personas.git", None, dir)
                .await
                .expect("clone_repo");
        }
        let flat = vec![
            ".agents/personas/broken.md".to_string(),
            ".agents/personas/developer.md".to_string(),
            ".agents/personas/link.md".to_string(),
        ];
        assert_eq!(persona_files(&pool, id, "personas").await, flat, "nested/x.md isn't a persona file");
        assert_eq!(persona_files(&pool, id, "again").await, flat);

        // Undecided: named, never read into the result.
        let told = crate::git::list_personas(&pool, id).await.expect("list_personas");
        assert!(told.contains("load_instructions"), "{told}");
        assert!(!told.contains("Works one ticket") && !told.contains("tools"), "{told}");

        db::set_repo_trust(&pool, "file/tmp/personas", true).await.expect("trust");
        let told = crate::git::list_personas(&pool, id).await.expect("list_personas");
        let listing: serde_json::Value = serde_json::from_str(&told).expect("json");
        for dir in ["personas", "again"] {
            let personas = personas_of(&listing, &format!("/workspace/{dir}"));
            let developer = personas.iter().find(|p| p["persona"] == format!("{dir}:developer")).expect("developer");
            assert_eq!(
                (developer["description"].as_str(), developer["model"].as_str()),
                (Some("Works one ticket."), Some("claude-opus-4-5")),
                "{told}"
            );
            let error = |file: &str| {
                let path = format!("/workspace/{dir}/.agents/personas/{file}");
                let entry = personas.iter().find(|p| p["file"] == path).unwrap_or_else(|| panic!("{path}: {told}"));
                entry["error"].as_str().unwrap_or_else(|| panic!("{path} has an error: {told}")).to_string()
            };
            assert!(error("broken.md").contains("unknown field `tools`"), "{told}");
            assert!(error("link.md").contains("symlink"), "{told}");
        }

        // A persona committed since the clone is listed, and recorded.
        sh(
            &client,
            pod,
            "set -e; cd /workspace/personas; \
             printf -- '---\\nname: reviewer\\ndescription: Reviews one PR.\\n---\\n' > .agents/personas/reviewer.md; \
             git add .agents/personas/reviewer.md; git -c user.name=t -c user.email=t@example.com commit -qm two",
        )
        .await;
        let told = crate::git::list_personas(&pool, id).await.expect("list_personas");
        assert!(told.contains("personas:reviewer"), "{told}");
        assert!(
            persona_files(&pool, id, "personas").await.contains(&".agents/personas/reviewer.md".to_string()),
            "re-recorded"
        );

        // "Work on a repo" records them too, and trusts the remote itself.
        make_origin_with_personas(&client, pod, "/tmp/named.git").await;
        crate::git::attach_repo(&pool, id, "file:///tmp/named.git", None).await.expect("attach_repo");
        assert_eq!(persona_files(&pool, id, "named").await, flat);
        let told = crate::git::list_personas(&pool, id).await.expect("list_personas");
        assert!(told.contains("named:developer"), "{told}");
    })
    .await;
}

/// SME-236: a child agent's checkout, cloned at a ref where its persona
/// differs from the default branch's, holds and reads that ref's persona.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_child_cloned_at_a_ref_reads_that_refs_persona(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let child = db::create_conversation(&pool).await.expect("create conversation").id;
        let pod = create_pod(&pool, child, PodLimitOverrides::default()).await.expect("create_pod");
        // main's developer.md says one thing; tag v2's says another.
        make_origin_with_personas(&client, pod, "/tmp/child.git").await;
        sh(
            &client,
            pod,
            "set -e; cd /tmp/src; printf -- '---\\nname: developer\\ndescription: The v2 one.\\n---\\nRead v2.\\n' > .agents/personas/developer.md; \
             git -c user.name=t -c user.email=t@example.com commit -qam v2; git tag v2; git reset -q --hard HEAD~1; \
             git push -q /tmp/child.git v2",
        )
        .await;
        let repo = db::create_conversation_repo(&pool, child, "file:///tmp/child.git", "file/tmp/child", None, "child")
            .await
            .expect("the child's repo row");

        let target = crate::git::CloneTarget::Ref("v2".to_string());
        crate::git::clone_child_repo(&pool, child, &repo, &target).await.expect("clone at v2");
        let ready = db::list_conversation_repos(&pool, child).await.expect("repos").remove(0);
        assert_eq!(ready.status, "ready", "{:?}", ready.error);
        assert!(ready.persona_files.contains(&".agents/personas/developer.md".to_string()), "{ready:?}");
        let checkout = crate::agents::PodCheckout;
        let read = crate::agents::ChildCheckout::read_file(&checkout, &pool, child, "child", ".agents/personas/developer.md")
            .await
            .expect("read the persona")
            .expect("it's there");
        assert!(read.content.contains("The v2 one.") && read.content.contains("Read v2."), "{}", read.content);
    })
    .await;
}
