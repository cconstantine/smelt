//! SME-242, T6: the install decision in a real pod. One conversation's
//! credential files are written at its pod's creation and at every
//! re-install, mode 600 and owned by sandbox, and both directories are
//! pruned to what the rules grant now — a narrowed chain loses its key
//! file, a deleted secret loses its file, while git's config and commit
//! identity (not credentials) stay.
//!
//! T7 and T13 are the child story: children started under personas that
//! name nothing, name `credentials: []`, name a stored secret, and a
//! grandchild whose own persona names a secret no ancestor held. The
//! pod is watched on each side of the start's boundary — at
//! `BEFORE_START_DONE` (after the pre-`complete_start` reconcile,
//! before the first turn can be woken) and once the start's task has
//! ended — which is what makes "no key at the first turn" a checked
//! fact rather than a race.

use super::*;
use crate::agents::test_hooks;

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

/// The fixture T7 and T13 share: a volume mounted into every pod of
/// the line (SME-239's, so a `file://` clone works in any pod), a
/// bare origin at `/data/shared/origin.git` whose one main commit
/// carries four persona files — `plain` (no `smelt` block at all),
/// `no-creds` (`credentials: []`), `keeper` (a stored secret's name)
/// and `deep` (another's) — and the root's own checkout of it,
/// trusted. Returns the root conversation.
async fn root_with_checkout(pool: &PgPool, client: &kube::Client) -> i64 {
    let root = db::create_conversation(pool).await.expect("a root").id;
    crate::agents::create_volume(pool, root, &serde_json::json!({ "name": "origin", "mount_path": "/data/shared" }))
        .await
        .expect("the origin volume");
    let pod = create_pod(pool, root, PodLimitOverrides::default()).await.expect("the root's pod");
    sh_ok(
        client,
        pod,
        r#"set -e
rm -rf /tmp/src /data/shared/origin.git && mkdir -p /tmp/src/.agents/personas
git init -q -b main /tmp/src && cd /tmp/src
git config user.name t && git config user.email t@example.com
printf -- '---\nname: plain\ndescription: d\n---\nWork plainly.\n' > .agents/personas/plain.md
printf -- '---\nname: no-creds\ndescription: d\nsmelt:\n  credentials: []\n---\nWork without credentials.\n' > .agents/personas/no-creds.md
printf -- '---\nname: keeper\ndescription: d\nsmelt:\n  credentials: [linear-key]\n---\nKeep the linear key.\n' > .agents/personas/keeper.md
printf -- '---\nname: deep\ndescription: d\nsmelt:\n  credentials: [deep-key]\n---\nGo deep.\n' > .agents/personas/deep.md
git add -A && git commit -qm init && git clone -q --bare /tmp/src /data/shared/origin.git"#,
    )
    .await;
    crate::git::clone_repo(pool, root, "file:///data/shared/origin.git", None, Some("origin")).await.expect("the root's checkout");
    let repos = db::list_conversation_repos(pool, root).await.expect("the root's repos");
    db::set_repo_trust(pool, &repos[0].remote_key, true).await.expect("trust");
    root
}

/// Starts a child of `caller` on the root checkout, with `persona`
/// when it has one, and returns its conversation id: `start_agent`
/// makes the row before spawning the start, so the newest child is
/// deterministically the one just made. The origin rides the shared
/// volume, so the child holds it the way its parent does.
async fn start_child(pool: &PgPool, caller: i64, persona: Option<&str>) -> i64 {
    let mut input = serde_json::json!({
        "brief": "work",
        "repo": format!("{}/origin", crate::sandbox::WORKSPACE_DIR),
        "volumes": ["origin"],
    });
    if let Some(name) = persona {
        input["persona"] = serde_json::Value::String(name.to_string());
    }
    crate::agents::start_agent(pool, caller, &input).await.expect("start_agent");
    sqlx::query_scalar::<_, i64>("SELECT max(id) FROM conversations WHERE parent_id = $1")
        .bind(caller)
        .fetch_one(pool)
        .await
        .expect("the child's row")
}

/// The child's `agents.credentials` column.
async fn column(pool: &PgPool, child: i64) -> Vec<String> {
    sqlx::query_scalar::<_, Vec<String>>("SELECT credentials FROM agents WHERE conversation_id = $1")
        .bind(child)
        .fetch_one(pool)
        .await
        .expect("the child's column")
}

/// Waits for child `id`'s state — and fails the test if it never gets
/// there, rather than hanging on the scenario's own bound.
async fn wait_for_state(pool: &PgPool, id: i64, want: &str) {
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let state: Option<String> = sqlx::query_scalar("SELECT state FROM agents WHERE conversation_id = $1")
                .bind(id)
                .fetch_optional(pool)
                .await
                .expect("state");
            if state.as_deref() == Some(want) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("child {id} never reached {want}"));
}

/// Waits for child `id`'s start task to have ended — the exit and its
/// clearing and reconcile included, which is what the assertions after
/// this one are about.
async fn wait_start_ended(child: i64) {
    tokio::time::timeout(Duration::from_secs(90), async {
        while !test_hooks::starts_ended(child) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the start task ended");
}

/// The conversation's live pod, appearing when its start's clone makes
/// it (or its first turn does).
async fn wait_pod_id(pool: &PgPool, conversation: i64) -> i64 {
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            if let Ok(pod) = live_pod_id(pool, conversation).await {
                return pod;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("conversation {conversation} never had a live pod"))
}

/// Pauses `caller`'s next child's start at its done boundary: awaiting
/// the second half waits for the pause, sending on the first lets the
/// start go on.
fn pause_at_done_boundary(caller: i64) -> (tokio::sync::oneshot::Sender<()>, impl Future<Output = ()>) {
    let (reached, release) = test_hooks::pause(test_hooks::BEFORE_START_DONE, caller);
    (release, async move {
        tokio::time::timeout(Duration::from_secs(90), reached).await.expect("the boundary").expect("the hook");
    })
}

/// SME-242 T7: the child story in real pods, watched at
/// `BEFORE_START_DONE` (after the pre-`complete_start` reconcile) and
/// once the start's task has ended. Three starts and an injected
/// failure, each with a pod: twice the usual scenario bound.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_childs_pod_holds_only_what_its_persona_left_it(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT * 2, async {
        db::create_ssh_key(&pool, "id_ed25519", "pub", "PRIVATE-MARKER").await.expect("a key");
        sqlx::query("INSERT INTO named_secrets (name, value) VALUES ('linear-key', 'SECRET-MARKER')")
            .execute(&pool)
            .await
            .expect("a secret");
        let root = root_with_checkout(&pool, &client).await;

        // (a) A persona-less child: Rule G's bit rides the whole start —
        // the key at the boundary and after — and `/etc/smelt/secrets`
        // stays empty however many secrets are stored.
        let (release, wait_paused) = pause_at_done_boundary(root);
        let kept = start_child(&pool, root, None).await;
        wait_paused.await;
        let pod = wait_pod_id(&pool, kept).await;
        sh_ok(&client, pod, "test -e /etc/smelt/keys/id_ed25519").await;
        assert_eq!(sh_ok(&client, pod, "grep -c IdentityFile /etc/smelt/ssh_config || true").await, "1");
        assert_eq!(sh_ok(&client, pod, "ls -A /etc/smelt/secrets").await, "", "no Rule S without a persona");
        release.send(()).expect("the start goes on");
        wait_for_state(&pool, kept, "active").await;
        wait_start_ended(kept).await;
        sh_ok(&client, pod, "test -e /etc/smelt/keys/id_ed25519").await;
        assert_eq!(sh_ok(&client, pod, "ls -A /etc/smelt/secrets").await, "");

        // (b) A `credentials: []` persona: the clone still worked — its
        // repo is `ready`, and at that moment the key was in the pod —
        // and by the boundary nothing of it is left, at the boundary
        // and after.
        let (release, wait_paused) = pause_at_done_boundary(root);
        let excluded = start_child(&pool, root, Some("no-creds")).await;
        wait_paused.await;
        let status: String =
            sqlx::query_scalar("SELECT status FROM conversation_repos WHERE conversation_id = $1")
                .bind(excluded)
                .fetch_one(&pool)
                .await
                .expect("its repo");
        assert_eq!(status, "ready", "the clone had the key it needed");
        let pod = wait_pod_id(&pool, excluded).await;
        let (code, _) = sh(&client, pod, "test -e /etc/smelt/keys/id_ed25519").await;
        assert_eq!(code, 1, "the key is gone before the child is active");
        assert_eq!(sh_ok(&client, pod, "grep -c IdentityFile /etc/smelt/ssh_config || true").await, "0");
        release.send(()).expect("the start goes on");
        wait_for_state(&pool, excluded, "active").await;
        wait_start_ended(excluded).await;
        let (code, _) = sh(&client, pod, "test -e /etc/smelt/keys/id_ed25519").await;
        assert_eq!(code, 1, "and stays gone");
        assert_eq!(column(&pool, excluded).await, Vec::<String>::new(), "the bit was narrowed away");

        // (c) An injected start failure: the child ends `failed` with no
        // key on the way out and no bit left to resume one with. (The
        // failure lands before the clone, so its pod is never made —
        // which is the strongest form of "leaves no key behind".)
        test_hooks::fail_next_start(root, "injected for T7");
        let cut_off = start_child(&pool, root, Some("no-creds")).await;
        wait_for_state(&pool, cut_off, "failed").await;
        wait_start_ended(cut_off).await;
        assert_eq!(column(&pool, cut_off).await, Vec::<String>::new(), "a cut-off start keeps no bit");
        let pods: i64 = sqlx::query_scalar("SELECT count(*) FROM sandbox_pods WHERE conversation_id = $1")
            .bind(cut_off)
            .fetch_one(&pool)
            .await
            .expect("its pods");
        assert_eq!(pods, 0, "no pod was ever made for it");

        // (d) A persona naming a stored secret gets exactly that file,
        // at 600, with the key gone (the persona excludes `git-ssh`) —
        // the reconcile adding, not just removing.
        let (release, wait_paused) = pause_at_done_boundary(root);
        let keeper = start_child(&pool, root, Some("keeper")).await;
        wait_paused.await;
        let pod = wait_pod_id(&pool, keeper).await;
        assert_eq!(sh_ok(&client, pod, "ls -A /etc/smelt/secrets").await, "linear-key");
        assert_eq!(sh_ok(&client, pod, "stat -c '%a %U' /etc/smelt/secrets/linear-key").await, "600 sandbox");
        let (code, _) = sh(&client, pod, "test -e /etc/smelt/keys/id_ed25519").await;
        assert_eq!(code, 1, "no key, as the persona asked");
        release.send(()).expect("the start goes on");
        wait_for_state(&pool, keeper, "active").await;
        wait_start_ended(keeper).await;

        // (e) The live-pod loop asks the same decision: saving the commit
        // identity — the cheapest key-path change — reaches every live
        // pod, and this one gets git's config and still no key file and
        // no SSH offer.
        crate::git::save_identity(&pool, &crate::git::GitIdentity { name: "U".into(), email: "u@example.com".into() })
            .await
            .expect("save_identity");
        sh_ok(&client, pod, "test -e /etc/smelt/gitconfig").await;
        assert_eq!(sh_ok(&client, pod, "grep -c IdentityFile /etc/smelt/ssh_config || true").await, "0");
        let (code, _) = sh(&client, pod, "test -e /etc/smelt/keys/id_ed25519").await;
        assert_eq!(code, 1, "the loop never re-grants");
        sh_ok(&client, pod, "test -e /etc/smelt/secrets/linear-key").await;
    })
    .await;
}

/// SME-242 T13: the exempt rule end to end. A root with `deep-key`
/// stored, a child under a persona that names nothing, and a
/// grandchild under a persona that names `deep-key`: the secret lands
/// at 600 in the grandchild's pod before its first turn and its
/// parent's pod never holds it — the secret reaches a depth its
/// ancestors never held, which is what the user chose. Three pods:
/// twice the usual scenario bound.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stored_secret_reaches_a_grandchild_no_ancestor_held(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT * 2, async {
        db::create_ssh_key(&pool, "id_ed25519", "pub", "PRIVATE-MARKER").await.expect("a key");
        sqlx::query("INSERT INTO named_secrets (name, value) VALUES ('deep-key', 'DEEP-SECRET')")
            .execute(&pool)
            .await
            .expect("a secret");
        let root = root_with_checkout(&pool, &client).await;

        // The child, under `plain`: names nothing, so it narrows nothing
        // away and holds the chain's key and no secrets — in particular
        // not `deep-key`, which is stored while its pod lives.
        let child = start_child(&pool, root, Some("plain")).await;
        wait_for_state(&pool, child, "active").await;
        wait_start_ended(child).await;
        let child_pod = wait_pod_id(&pool, child).await;
        let (code, _) = sh(&client, child_pod, "test -e /etc/smelt/secrets/deep-key").await;
        assert_eq!(code, 1, "its parent holds nothing");

        // The grandchild, under `deep`: its own persona names the
        // secret, so it arrives at its boundary — before its first turn
        // — and its chain bit (the persona asks no `git-ssh`) narrows
        // away.
        let (release, wait_paused) = pause_at_done_boundary(child);
        let grand = start_child(&pool, child, Some("deep")).await;
        wait_paused.await;
        let grand_pod = wait_pod_id(&pool, grand).await;
        assert_eq!(
            sh_ok(&client, grand_pod, "stat -c '%a %U' /etc/smelt/secrets/deep-key").await,
            "600 sandbox",
            "the secret reaches a depth no ancestor held"
        );
        assert_eq!(column(&pool, grand).await, Vec::<String>::new(), "`deep` asks no `git-ssh`");
        release.send(()).expect("the start goes on");
        wait_for_state(&pool, grand, "active").await;
        wait_start_ended(grand).await;
        sh_ok(&client, grand_pod, "test -e /etc/smelt/secrets/deep-key").await;
        let (code, _) = sh(&client, child_pod, "test -e /etc/smelt/secrets/deep-key").await;
        assert_eq!(code, 1, "and the parent's pod still holds nothing");
    })
    .await;
}
