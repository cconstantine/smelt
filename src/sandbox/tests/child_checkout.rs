//! A child agent's checkout against a real pod (SME-236): `checkout_ref`
//! after a clone of the default branch, at a branch, a tag, a commit no
//! branch tip is at, and one no ref reaches, which is fetched by id.

use super::*;
use crate::git::CloneTarget;

const ORIGIN: &str = "file:///tmp/origin.git";

/// Makes /tmp/origin.git in the pod: `main` at c1 -> c2 -> c3, `feature`
/// off c1, tag `v1` at c1, and a commit only a since-deleted branch held.
/// Prints c1, c2, c3, feature's commit and the unreferenced one.
async fn make_origin(sandbox: &Sandbox) -> Vec<String> {
    let script = r#"set -e
        c() { git -c user.name=t -c user.email=t@t commit -q --allow-empty -m "$1"; git rev-parse HEAD; }
        git init -q -b main /tmp/src && cd /tmp/src
        c1=$(c one); git tag v1
        git checkout -q -b feature; f=$(c feature)
        git checkout -q main; c2=$(c two); c3=$(c three)
        git checkout -q -b gone "$c1"; g=$(c gone); git checkout -q main
        git clone -q --bare /tmp/src /tmp/origin.git
        git -C /tmp/origin.git branch -q -D gone
        echo "$c1"; echo "$c2"; echo "$c3"; echo "$f"; echo "$g""#;
    let made = sandbox.exec(&["sh", "-c", script]).await.expect("exec make origin");
    assert_eq!(made.exit_code, 0, "make origin: {}{}", made.stdout, made.stderr);
    made.stdout.lines().map(str::to_string).collect()
}

/// `(HEAD's commit, its branch or "HEAD" when detached, its upstream or "")`.
async fn head_of(sandbox: &Sandbox, dir: &str) -> (String, String, String) {
    let script = r#"cd "$1" && git rev-parse HEAD && (git symbolic-ref --short -q HEAD || echo HEAD)
        ref=$(git symbolic-ref -q HEAD) && git for-each-ref --format='%(upstream:short)' "$ref" || echo"#;
    let path = format!("/workspace/{dir}");
    let read = sandbox.exec(&["sh", "-c", script, "sh", &path]).await.expect("exec head");
    let lines: Vec<String> = read.stdout.lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 3, "{}{}", read.stdout, read.stderr);
    (lines[0].clone(), lines[1].clone(), lines[2].clone())
}

#[tokio::test]
async fn test_checkout_ref_takes_a_branch_a_tag_and_commits() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let sandbox = manager.create(&unique_session_id("child-ref"), "256Mi", &[]).await.expect("create pod");
    let shell = sandbox.shell();

    let checks = tokio::time::timeout(Duration::from_secs(120), async {
        let made = make_origin(&sandbox).await;
        let (c1, c2, feature, gone) = (&made[0], &made[1], &made[3], &made[4]);
        let at = |dir: &'static str, target: CloneTarget| {
            let shell = &shell;
            async move {
                crate::git::clone_into_pod(shell, ORIGIN, None, dir).await.expect("clone the default branch");
                crate::git::checkout_ref(shell, dir, ORIGIN, &target).await
            }
        };

        at("branch", CloneTarget::Ref("feature".to_string())).await.expect("a branch");
        assert_eq!(head_of(&sandbox, "branch").await, (feature.clone(), "feature".to_string(), "origin/feature".to_string()));

        at("tag", CloneTarget::Ref("v1".to_string())).await.expect("a tag");
        assert_eq!(head_of(&sandbox, "tag").await, (c1.clone(), "HEAD".to_string(), String::new()), "detached at the tag");

        at("commit", CloneTarget::Ref(c2.clone())).await.expect("a commit no tip is at");
        assert_eq!(head_of(&sandbox, "commit").await, (c2.clone(), "HEAD".to_string(), String::new()));

        at("short", CloneTarget::Ref(c2[..10].to_string())).await.expect("an abbreviated commit");
        assert_eq!(head_of(&sandbox, "short").await.0, *c2);

        at("fetched", CloneTarget::Ref(gone.clone())).await.expect("a commit no ref reaches, fetched by id");
        assert_eq!(head_of(&sandbox, "fetched").await.0, *gone);

        let unknown = at("unknown", CloneTarget::Ref("nope".to_string())).await.expect_err("refused");
        assert_eq!(unknown, format!("no branch, tag or commit `nope` on {ORIGIN}"));
        let missing = at("missing", CloneTarget::Ref("0123456789abcdef0123456789abcdef01234567".to_string()))
            .await
            .expect_err("refused");
        assert!(missing.starts_with("no branch, tag or commit `0123456789"), "{missing}");
    })
    .await;

    manager.delete(sandbox).await.expect("delete pod");
    checks.expect("checks finished within the timeout");
}

/// A parent's checkout of /tmp/origin.git at /workspace/<dir>, on `main`
/// tracking origin's, after `script` runs in it.
async fn parent_checkout(sandbox: &Sandbox, shell: &crate::sandbox::PodShell, dir: &'static str, script: &str) {
    crate::git::clone_into_pod(shell, ORIGIN, None, dir).await.expect("clone");
    let path = format!("/workspace/{dir}");
    let full = format!("set -e; cd \"$1\"; c() {{ git -c user.name=t -c user.email=t@t commit -q --allow-empty -m \"$1\"; }}\n{script}");
    let ran = sandbox.exec(&["sh", "-c", &full, "sh", &path]).await.expect("exec setup");
    assert_eq!(ran.exit_code, 0, "{dir}: {}{}", ran.stdout, ran.stderr);
}

/// The parent's commit: given when it's on its upstream and clean (an
/// untracked file aside), refused with what to do otherwise; and a child
/// checked out at it is on a branch of the upstream's name.
#[tokio::test]
async fn test_parent_commit_needs_a_pushed_clean_branch() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let sandbox = manager.create(&unique_session_id("parent-commit"), "256Mi", &[]).await.expect("create pod");
    let shell = sandbox.shell();

    let checks = tokio::time::timeout(Duration::from_secs(120), async {
        let made = make_origin(&sandbox).await;
        let (c2, feature) = (&made[1], &made[3]);

        // Behind its upstream (reset to c2, origin/main at c3), with an
        // untracked file: its own commit, upstream `main`.
        parent_checkout(&sandbox, &shell, "behind", "git reset -q --hard HEAD~1; touch untracked.txt").await;
        assert_eq!(crate::git::parent_commit(&shell, "behind").await, Ok((c2.clone(), "main".to_string())));

        parent_checkout(&sandbox, &shell, "on-feature", "git checkout -q feature").await;
        assert_eq!(crate::git::parent_commit(&shell, "on-feature").await, Ok((feature.clone(), "feature".to_string())));

        parent_checkout(&sandbox, &shell, "detached", "git checkout -q --detach HEAD").await;
        let refused = crate::git::parent_commit(&shell, "detached").await.expect_err("detached");
        assert!(refused.starts_with("/workspace/detached is on a detached HEAD"), "{refused}");

        parent_checkout(&sandbox, &shell, "no-upstream", "git checkout -q -b local").await;
        let refused = crate::git::parent_commit(&shell, "no-upstream").await.expect_err("no upstream");
        assert!(refused.starts_with("/workspace/no-upstream's branch local has no upstream: push it (git push -u origin local)"), "{refused}");

        // Review 1, finding 5: an upstream that isn't a branch on origin (a
        // local one here) is refused, not checked against a stale ref; and
        // an upstream whose name holds a slash keeps it.
        parent_checkout(&sandbox, &shell, "local-upstream", "git checkout -q -b work --track main; c unpushed").await;
        let refused = crate::git::parent_commit(&shell, "local-upstream").await.expect_err("a local upstream");
        assert!(
            refused.starts_with("/workspace/local-upstream's branch work tracks main, not a branch on origin"),
            "{refused}"
        );
        parent_checkout(&sandbox, &shell, "slashed", "git push -q origin HEAD:refs/heads/feat/x; git fetch -q origin; git checkout -q -b mine --track origin/feat/x").await;
        let (_, upstream) = crate::git::parent_commit(&shell, "slashed").await.expect("a pushed branch");
        assert_eq!(upstream, "feat/x");

        parent_checkout(&sandbox, &shell, "ahead", "c mine; c mine-too").await;
        let refused = crate::git::parent_commit(&shell, "ahead").await.expect_err("ahead");
        assert!(refused.starts_with("/workspace/ahead has 2 commits not on origin/main: push them"), "{refused}");

        parent_checkout(&sandbox, &shell, "dirty", "echo x > tracked.txt; git add tracked.txt; c add; git push -q origin HEAD:main; echo y > tracked.txt").await;
        let refused = crate::git::parent_commit(&shell, "dirty").await.expect_err("dirty");
        assert!(refused.starts_with("/workspace/dirty has uncommitted changes to tracked files"), "{refused}");

        // A child at the parent's commit: on `main`, at c2, tracking origin's.
        crate::git::clone_into_pod(&shell, ORIGIN, None, "child").await.expect("clone");
        let target = CloneTarget::Commit { sha: c2.clone(), branch: "main".to_string() };
        crate::git::checkout_ref(&shell, "child", ORIGIN, &target).await.expect("check out the parent's commit");
        assert_eq!(head_of(&sandbox, "child").await, (c2.clone(), "main".to_string(), "origin/main".to_string()));
    })
    .await;

    manager.delete(sandbox).await.expect("delete pod");
    checks.expect("checks finished within the timeout");
}
