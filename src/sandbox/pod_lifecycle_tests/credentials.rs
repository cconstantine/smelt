//! SME-242, T6: the install decision in a real pod. One conversation's
//! credential files are written at its pod's creation and at every
//! re-install, mode 600 and owned by sandbox, and both directories are
//! pruned to what the rules grant now — a narrowed chain loses its key
//! file, a deleted secret loses its file, while git's config and commit
//! identity (not credentials) stay.

use super::*;

/// Runs `script` in pod `pod`'s sandbox container and returns its
/// stdout, trimmed; the caller says whether exit 0 is expected.
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

/// An agent row for `conversation`: chain bit `git-ssh` and the
/// persona snapshot naming `linear-key` — the granted shape.
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_real_pod_holds_only_what_its_conversation_is_granted(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        db::create_ssh_key(&pool, "id_ed25519", "pub", "PRIVATE-MARKER").await.expect("a key");
        sqlx::query("INSERT INTO named_secrets (name, value) VALUES ('linear-key', 'SECRET-MARKER')")
            .execute(&pool)
            .await
            .expect("a secret");

        // A persona agent granted the key and the secret: its pod's
        // creation installs both, at 600, offered and pruned-to-name.
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        persona_agent(&pool, conversation.id).await;
        let pod = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        let modes = sh_ok(&client, pod, "stat -c '%a %U' /etc/smelt/keys/id_ed25519 /etc/smelt/secrets/linear-key").await;
        assert_eq!(modes, "600 sandbox\n600 sandbox", "key and secret at 600, owned by sandbox");
        let offered = sh_ok(&client, pod, "grep -c IdentityFile /etc/smelt/ssh_config || true").await;
        assert_eq!(offered, "1", "exactly one IdentityFile line");
        assert_eq!(sh_ok(&client, pod, "ls -A /etc/smelt/secrets").await, "linear-key", "only that name");

        // The same pod re-installed for a conversation granted neither:
        // the key file and the secret file are gone, `ssh_config` keeps
        // no IdentityFile line, and `gitconfig` — the commit identity,
        // not a credential — is still there.
        sqlx::query("UPDATE agents SET credentials = '{}', persona_settings = '{\"credentials\": []}'")
            .execute(&pool)
            .await
            .expect("narrow");
        crate::credentials::install_into_pod(&pool, pod).await.expect("reinstall");
        let (code, _) = sh(&client, pod, "test -e /etc/smelt/keys/id_ed25519").await;
        assert_eq!(code, 1, "the key file was pruned");
        let (code, _) = sh(&client, pod, "test -e /etc/smelt/secrets/linear-key").await;
        assert_eq!(code, 1, "the secret file was pruned");
        let offered = sh_ok(&client, pod, "grep -c IdentityFile /etc/smelt/ssh_config || true").await;
        assert_eq!(offered, "0", "the SSH config offers nothing");
        sh_ok(&client, pod, "test -e /etc/smelt/gitconfig").await;

        // The name listed but the store not holding it: an unbacked
        // name writes no file. Delete the row, list the name, install.
        sqlx::query("DELETE FROM named_secrets WHERE name = 'linear-key'").execute(&pool).await.expect("delete");
        sqlx::query("UPDATE agents SET persona_settings = '{\"credentials\": [\"linear-key\"]}'")
            .execute(&pool)
            .await
            .expect("name it again");
        crate::credentials::install_into_pod(&pool, pod).await.expect("still unbacked, still nothing");
        let (code, _) = sh(&client, pod, "test -e /etc/smelt/secrets/linear-key").await;
        assert_eq!(code, 1, "an unbacked name writes no file");
        // Storing it again (no change to the persona file) installs the
        // file at the next install — Rule S's backing is live (Trade-off 3).
        sqlx::query("INSERT INTO named_secrets (name, value) VALUES ('linear-key', 'SECRET-MARKER')")
            .execute(&pool)
            .await
            .expect("a secret again");
        crate::credentials::install_into_pod(&pool, pod).await.expect("backed, installed");
        sh_ok(&client, pod, "test -e /etc/smelt/secrets/linear-key").await;
        // And deleting it prunes the file from the live pod.
        sqlx::query("DELETE FROM named_secrets WHERE name = 'linear-key'").execute(&pool).await.expect("delete");
        crate::credentials::install_into_pod(&pool, pod).await.expect("reinstall after delete");
        let (code, _) = sh(&client, pod, "test -e /etc/smelt/secrets/linear-key").await;
        assert_eq!(code, 1, "a deleted secret's file is pruned");
    })
    .await;
}

/// The user's own conversation — no agents row, so no persona and no
/// Rule S — gets the key as every pod has today, and its secrets
/// directory stays empty however many are stored.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_root_pod_gets_the_key_and_holds_no_secret_files(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        db::create_ssh_key(&pool, "id_ed25519", "pub", "PRIVATE-MARKER").await.expect("a key");
        sqlx::query("INSERT INTO named_secrets (name, value) VALUES ('linear-key', 'SECRET-MARKER')")
            .execute(&pool)
            .await
            .expect("a secret");
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let pod = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        sh_ok(&client, pod, "test -e /etc/smelt/keys/id_ed25519").await;
        let listed = sh_ok(&client, pod, "ls -A /etc/smelt/secrets").await;
        assert_eq!(listed, "", "the root holds no secret files");
    })
    .await;
}
