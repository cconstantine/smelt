//! SME-440: an install's writes into a real pod under stalls. An exec
//! that never answers is run again; a write lands whole or not at all.

use super::*;

/// Runs `script` in pod `pod`'s sandbox container: exit code and stdout.
async fn sh(client: &kube::Client, pod: i64, script: &str) -> (i32, String) {
    let ran = exec_with(client, &pod_name(pod), "sandbox", &["sh", "-c", script], None)
        .await
        .expect("exec");
    (ran.exit_code, ran.stdout.trim().to_string())
}

async fn sh_ok(client: &kube::Client, pod: i64, script: &str) -> String {
    let (code, out) = sh(client, pod, script).await;
    assert_eq!(code, 0, "{script}: {out}");
    out
}

/// An agent row for `conversation`: chain bit `git-ssh` and the persona
/// snapshot naming `linear-key`.
async fn persona_agent(pool: &PgPool, conversation: i64) {
    sqlx::query(
        "INSERT INTO agents (conversation_id, root_id, depth, brief, state, credentials, persona_settings)
         VALUES ($1, $1, 1, 'b', 'active', ARRAY['git-ssh'], '{\"credentials\": [\"linear-key\"]}')",
    )
    .bind(conversation)
    .execute(pool)
    .await
    .expect("an agent row");
}

/// SME-440: an install whose exec stalls still lands. On a loaded CI
/// runner one exec of a reinstall never answered, and the whole install
/// failed at `INSTALL_TIMEOUT` with the narrowed key and secret still in
/// the pod. Here the first exec of the reinstall runs and then never
/// answers: the install must still finish, with both files pruned.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_install_whose_exec_stalls_still_lands(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        db::create_ssh_key(&pool, "id_ed25519", "pub", "PRIVATE-MARKER").await.expect("a key");
        sqlx::query("INSERT INTO named_secrets (name, value) VALUES ('linear-key', 'SECRET-MARKER')")
            .execute(&pool)
            .await
            .expect("a secret");
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        persona_agent(&pool, conversation.id).await;
        let pod = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        sh_ok(&client, pod, "test -e /etc/smelt/secrets/linear-key").await;

        sqlx::query("UPDATE agents SET credentials = '{}', persona_settings = '{\"credentials\": []}'")
            .execute(&pool)
            .await
            .expect("narrow");
        crate::sandbox::stall_hook::stall_next(&pod_name(pod), 1);
        let installed = crate::credentials::install_into_pod(&pool, pod).await;
        let unused = crate::sandbox::stall_hook::remaining(&pod_name(pod));
        crate::sandbox::stall_hook::stall_next(&pod_name(pod), 0);
        assert_eq!(unused, 0, "the stall was armed but no exec took it");
        installed.expect("an install with one stalled exec");
        let (code, _) = sh(&client, pod, "test -e /etc/smelt/keys/id_ed25519").await;
        assert_eq!(code, 1, "the key file was pruned");
        let (code, _) = sh(&client, pod, "test -e /etc/smelt/secrets/linear-key").await;
        assert_eq!(code, 1, "the secret file was pruned");
    })
    .await;
}

/// SME-440: a write whose stdin ends short — an exec dropped mid-write —
/// renames nothing over the file it would replace.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_short_write_never_replaces_the_file(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let pod = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        let write = |content: &'static str, length: &'static str| {
            let client = client.clone();
            async move {
                let script = crate::credentials::WRITE_FILE_SCRIPT;
                exec_with(&client, &pod_name(pod), "sandbox", &["sh", "-c", script, "sh", "/tmp/sme440", "600", length], Some(content.as_bytes()))
                    .await
                    .expect("exec")
                    .exit_code
            }
        };
        assert_eq!(write("GOOD-VALUE", "10").await, 0, "a whole write lands");
        assert_ne!(write("GOOD", "10").await, 0, "a short write fails");
        assert_eq!(sh_ok(&client, pod, "cat /tmp/sme440").await, "GOOD-VALUE", "and leaves the good file");
        assert_eq!(sh_ok(&client, pod, "stat -c %a /tmp/sme440").await, "600");
    })
    .await;
}

/// SME-440 review M1: two writes of one file at once — an abandoned try
/// and a later writer — each land whole or not at all. Here the first's
/// stdin arrives late, while the second writes a longer value: sharing one
/// temp name, the first's bytes went into the second's renamed file.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_two_writes_of_one_file_never_mix(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let pod = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        let harness = r#"S="$1"
(sleep 2; printf AAAAA) | sh -c "$S" sh /tmp/sme440m1 600 5 &
sleep 1
printf BBBBBBBBBB | sh -c "$S" sh /tmp/sme440m1 600 10
wait
cat /tmp/sme440m1"#;
        let ran = exec_with(&client, &pod_name(pod), "sandbox", &["sh", "-c", harness, "sh", crate::credentials::WRITE_FILE_SCRIPT], None)
            .await
            .expect("exec");
        let landed = ran.stdout.trim();
        assert!(landed == "AAAAA" || landed == "BBBBBBBBBB", "a mixed file landed: {landed:?}");
    })
    .await;
}
