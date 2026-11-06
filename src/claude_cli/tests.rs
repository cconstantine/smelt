//! The CLI's environment, binary and status (SME-429), against a fake
//! `claude`: a shell script each test writes into a directory of its own.
//! No test runs the real CLI or reaches Anthropic.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;

/// A directory of the test's own under the system temp dir, emptied first.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("smelt-claude-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("make the scratch dir");
    dir
}

/// Writes an executable `claude` into `dir` running `body` under `/bin/sh`.
///
/// Written by a child `sh`, never by this process: a file this process
/// holds open for writing is inherited by every child another test thread
/// forks meanwhile, until that child execs, and running the fake during
/// that window fails with ETXTBSY ("Text file busy"), which a stress run of
/// these tests hit about once in five runs (review 1).
fn fake_claude(dir: &Path, body: &str) -> PathBuf {
    use std::io::Write;
    let path = dir.join("claude");
    let mut writer = std::process::Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(&path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("start the fake's writer");
    let mut stdin = writer.stdin.take().expect("the writer's stdin");
    stdin.write_all(format!("#!/bin/sh\n{body}\n").as_bytes()).expect("write the fake claude");
    drop(stdin);
    assert!(writer.wait().expect("the writer finishes").success(), "the fake was written");
    path
}

/// A fake that answers `--version` and `auth status` with `auth` (its
/// stdout) and `auth_exit`.
fn answering(dir: &Path, auth: &str, auth_exit: i32) -> PathBuf {
    fake_claude(
        dir,
        &format!(
            "case \"$1\" in\n  --version) echo '2.1.318 (Claude Code)'; echo 'second line';;\n  auth) printf '%s' '{auth}'; exit {auth_exit};;\n  *) exit 2;;\nesac"
        ),
    )
}

/// What smelt's own environment looks like on a dev host: the two
/// `ANTHROPIC_*` the dev `.env` sets, a key, the database, and the ones
/// the CLI is allowed.
fn smelt_env(config_dir: Option<&Path>) -> impl Fn(&str) -> Option<OsString> {
    let mut vars: BTreeMap<&'static str, OsString> = BTreeMap::new();
    vars.insert("ANTHROPIC_BASE_URL", "http://proxy.invalid".into());
    vars.insert("ANTHROPIC_AUTH_TOKEN", "fake-auth-token".into());
    vars.insert("ANTHROPIC_API_KEY", "fake-api-key".into());
    vars.insert("DATABASE_URL", "postgres://fake@db.invalid/smelt".into());
    vars.insert("CLAUDE_CODE_OAUTH_TOKEN", "fake-oauth-token".into());
    vars.insert("HOME", "/home/smelt".into());
    vars.insert("PATH", std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()));
    if let Some(dir) = config_dir {
        vars.insert(CONFIG_DIR_VAR, dir.as_os_str().to_owned());
    }
    move |name| vars.get(name).cloned()
}

fn names(env: &[(String, OsString)]) -> BTreeSet<String> {
    env.iter().map(|(name, _)| name.clone()).collect()
}

#[test]
fn test_the_cli_env_passes_only_the_allowlist() {
    let env = cli_env(smelt_env(Some(Path::new("/var/smelt-claude"))));
    let expected: BTreeSet<String> = ["PATH", "HOME", "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CONFIG_DIR", "DISABLE_AUTOUPDATER"]
        .into_iter()
        .map(str::to_string)
        .collect();
    // Names only: the values are never printed.
    assert_eq!(names(&env), expected);
    let value = |name: &str| env.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
    assert_eq!(value("CLAUDE_CONFIG_DIR"), Some(OsString::from("/var/smelt-claude")), "SMELT_CLAUDE_CONFIG_DIR names it");
    assert_eq!(value("DISABLE_AUTOUPDATER"), Some(OsString::from("1")));
}

#[test]
fn test_the_cli_config_dir_defaults_and_unset_vars_stay_unset() {
    // Review 1 L5: under smelt's own home, never a shared /tmp.
    let env = cli_env(|name| match name {
        "PATH" => Some(OsString::from("/usr/bin")),
        "HOME" => Some(OsString::from("/home/smelt")),
        _ => None,
    });
    let set = names(&env);
    assert!(!set.contains("CLAUDE_CODE_OAUTH_TOKEN"), "unset in smelt, unset for the CLI");
    let config = env.iter().find(|(n, _)| n == "CLAUDE_CONFIG_DIR").map(|(_, v)| v.clone());
    assert_eq!(config, Some(OsString::from("/home/smelt/.local/state/smelt-claude")));
    // With no home and no SMELT_CLAUDE_CONFIG_DIR there's no default.
    let homeless = cli_env(|name| (name == "PATH").then(|| OsString::from("/usr/bin")));
    assert!(!names(&homeless).contains("HOME") && !names(&homeless).contains("CLAUDE_CONFIG_DIR"));
}

/// Review 1 L5: the config directory is smelt's alone: made mode 0700, and
/// an existing one is made 0700 too (which only its owner can do).
#[tokio::test]
async fn test_the_cli_config_dir_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("private");
    let claude = answering(&dir, r#"{"loggedIn":true}"#, 0);
    let binary = claude.to_str().expect("utf-8 path");
    let fresh = dir.join("state/smelt-claude");
    status_with(binary, cli_env(smelt_env(Some(&fresh))), Duration::from_secs(10)).await;
    let mode = |path: &Path| std::fs::metadata(path).expect("the dir").permissions().mode() & 0o777;
    assert_eq!(mode(&fresh), 0o700, "made private");
    let open = dir.join("open");
    std::fs::create_dir(&open).expect("make it");
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).expect("open it up");
    let status = status_with(binary, cli_env(smelt_env(Some(&open))), Duration::from_secs(10)).await;
    assert_eq!(mode(&open), 0o700, "made private: {status:?}");
    let homeless = cli_env(|name| (name == "PATH").then(|| std::env::var_os("PATH")).flatten());
    let status = status_with(binary, homeless, Duration::from_secs(10)).await;
    assert!(status.error.as_deref().is_some_and(|e| e.contains("SMELT_CLAUDE_CONFIG_DIR")), "{status:?}");
}

#[tokio::test]
async fn test_the_fake_cli_sees_none_of_smelts_environment() {
    let dir = scratch("env");
    let seen = dir.join("seen-env");
    let claude = fake_claude(&dir, &format!("env > '{}'\necho '2.1.318 (Claude Code)'", seen.display()));
    let config = dir.join("config");
    status_with(claude.to_str().expect("utf-8 path"), cli_env(smelt_env(Some(&config))), Duration::from_secs(10)).await;
    let dumped = std::fs::read_to_string(&seen).expect("the fake wrote its environment");
    let seen_names: BTreeSet<&str> = dumped.lines().filter_map(|line| line.split_once('=').map(|(n, _)| n)).collect();
    let leaked: Vec<&&str> = seen_names
        .iter()
        .filter(|name| name.starts_with("ANTHROPIC_") || **name == "DATABASE_URL")
        .collect();
    assert!(leaked.is_empty(), "these reached the CLI: {leaked:?}");
    // What sh adds itself (PWD, SHLVL, _) aside, it saw the allowlist only.
    let extra: Vec<&&str> = seen_names
        .iter()
        .filter(|name| !["PWD", "OLDPWD", "SHLVL", "_"].contains(name))
        .filter(|name| !["PATH", "HOME", "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CONFIG_DIR", "DISABLE_AUTOUPDATER"].contains(name))
        .collect();
    assert!(extra.is_empty(), "not on the allowlist: {extra:?}");
    assert!(config.is_dir(), "the config dir is made for the CLI");
}

#[tokio::test]
async fn test_the_status_says_version_sign_in_and_plan_and_nothing_else() {
    let dir = scratch("signed-in");
    let auth = r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"someone@example.com","orgName":"Org","subscriptionType":"max","token":"sk-ant-oat01-NEVERSHOWN"}"#;
    let claude = answering(&dir, auth, 0);
    let status = status_with(claude.to_str().expect("utf-8 path"), cli_env(smelt_env(Some(&dir.join("c")))), Duration::from_secs(10)).await;
    assert_eq!(
        status,
        ClaudeCliStatus {
            version: Some("2.1.318 (Claude Code)".to_string()),
            signed_in: true,
            auth_method: Some("claude.ai".to_string()),
            plan: Some("max".to_string()),
            error: None,
        }
    );
    let shown = format!("{status:?}");
    assert!(!shown.contains("sk-ant") && !shown.contains("example.com"), "only the version and two words cross");
}

#[tokio::test]
async fn test_the_status_says_signed_out_when_the_cli_does() {
    let dir = scratch("signed-out");
    let claude = answering(&dir, r#"{"loggedIn":false,"authMethod":"none"}"#, 1);
    let status = status_with(claude.to_str().expect("utf-8 path"), cli_env(smelt_env(Some(&dir.join("c")))), Duration::from_secs(10)).await;
    assert_eq!(status.version.as_deref(), Some("2.1.318 (Claude Code)"), "{status:?}");
    assert!(!status.signed_in);
    assert_eq!(status.auth_method.as_deref(), Some("none"));
    assert_eq!(status.error, None, "signed out is an answer, not an error");
}

#[tokio::test]
async fn test_an_unreadable_auth_status_is_an_error_that_echoes_nothing() {
    let dir = scratch("garbage");
    let claude = answering(&dir, "token: sk-ant-oat01-LEAKED", 0);
    let status = status_with(claude.to_str().expect("utf-8 path"), cli_env(smelt_env(Some(&dir.join("c")))), Duration::from_secs(10)).await;
    assert!(!status.signed_in);
    let error = status.error.expect("an error");
    assert!(error.contains("auth status"), "{error}");
    assert!(!error.contains("LEAKED") && !error.contains("sk-ant"), "the CLI's output is never echoed");
}

#[tokio::test]
async fn test_a_missing_binary_says_where_it_looked() {
    let dir = scratch("missing");
    let env = cli_env(smelt_env(Some(&dir.join("c"))));
    let by_path = status_with("/nonexistent/claude", env.clone(), Duration::from_secs(10)).await;
    assert_eq!(by_path.version, None);
    assert!(by_path.error.as_deref().is_some_and(|e| e.contains("/nonexistent/claude")), "{by_path:?}");
    let by_name = status_with("no-such-claude-binary", env, Duration::from_secs(10)).await;
    assert!(by_name.error.as_deref().is_some_and(|e| e.contains("PATH")), "{by_name:?}");
}

#[tokio::test]
async fn test_a_hung_cli_times_out() {
    let dir = scratch("hung");
    let claude = fake_claude(&dir, "sleep 30");
    let started = std::time::Instant::now();
    let status = status_with(claude.to_str().expect("utf-8 path"), cli_env(smelt_env(Some(&dir.join("c")))), Duration::from_millis(300)).await;
    assert!(started.elapsed() < Duration::from_secs(10), "it gave up rather than waiting");
    assert!(status.error.as_deref().is_some_and(|e| e.contains("didn't answer")), "{status:?}");
}

#[test]
fn test_a_binary_path_is_checked() {
    assert_eq!(checked_binary("  "), Ok(DEFAULT_BINARY.to_string()), "blank is the default");
    assert_eq!(checked_binary(" /opt/claude/bin/claude "), Ok("/opt/claude/bin/claude".to_string()));
    assert!(checked_binary("https://api.anthropic.com").is_err(), "an address isn't a binary");
    assert!(checked_binary("claude --dangerously").is_err(), "no arguments");
}

#[test]
fn test_the_status_line_says_signed_in_how_and_on_what_plan() {
    let status = |signed_in, auth_method: Option<&str>, plan: Option<&str>| ClaudeCliStatus {
        version: Some("2.1.318 (Claude Code)".to_string()),
        signed_in,
        auth_method: auth_method.map(str::to_string),
        plan: plan.map(str::to_string),
        error: None,
    };
    assert_eq!(
        status(true, Some("oauth_token"), Some("max")).summary(),
        "Claude Code 2.1.318 (Claude Code): signed in with CLAUDE_CODE_OAUTH_TOKEN, on the max plan."
    );
    assert_eq!(
        status(true, Some("claude.ai"), None).summary(),
        "Claude Code 2.1.318 (Claude Code): signed in with a claude.ai login; it didn't say which plan."
    );
    let signed_out = status(false, Some("none"), None).summary();
    assert!(signed_out.starts_with("Claude Code 2.1.318 (Claude Code): not signed in."), "{signed_out}");
    assert!(signed_out.contains("claude setup-token") && signed_out.contains("CLAUDE_CODE_OAUTH_TOKEN"), "says how: {signed_out}");
    let unread = ClaudeCliStatus { error: Some("claude auth status didn't answer within 10 seconds.".to_string()), ..status(false, None, None) };
    assert_eq!(unread.summary(), "Claude Code 2.1.318 (Claude Code): couldn't read its sign-in: claude auth status didn't answer within 10 seconds.");
    let missing = ClaudeCliStatus { error: Some("There's no claude on the server's PATH.".to_string()), ..ClaudeCliStatus::default() };
    assert_eq!(missing.summary(), "Couldn't run Claude Code: There's no claude on the server's PATH.");
}

/// Review 1 L1: a relative path would be checked from smelt's working
/// directory and run from the CLI's, so only an absolute one is taken.
#[tokio::test]
async fn test_a_relative_binary_path_is_refused() {
    assert!(checked_binary("bin/claude").is_err_and(|e| e.contains("absolute")), "the form refuses it");
    assert!(checked_binary("./claude").is_err(), "./ too");
    let dir = scratch("relative");
    let status = status_with("bin/claude", cli_env(smelt_env(Some(&dir.join("c")))), Duration::from_secs(10)).await;
    assert!(status.error.as_deref().is_some_and(|e| e.contains("absolute")), "a stored one is refused too: {status:?}");
}

/// Review 1 L2: output that isn't JSON says nothing about the sign-in,
/// whatever the exit code, so it's an error and never "not signed in"
/// (whose advice, to set a token, is wrong when one is set).
#[tokio::test]
async fn test_auth_status_output_that_isnt_json_is_unknown_even_on_a_failure() {
    let dir = scratch("not-json-failed");
    let claude = answering(&dir, "Unknown command: auth", 1);
    let status = status_with(claude.to_str().expect("utf-8 path"), cli_env(smelt_env(Some(&dir.join("c")))), Duration::from_secs(10)).await;
    assert!(!status.signed_in);
    let error = status.error.clone().expect("an error, not a sign-in answer");
    assert!(error.contains("didn't say whether it's signed in"), "{error}");
    assert!(!status.summary().contains("not signed in"), "{}", status.summary());
}

/// Review 1 L3: a page calling the status in a loop can't pile up CLI
/// processes: at most `MAX_STATUS_CHECKS` checks run at once.
#[tokio::test]
async fn test_status_checks_run_a_few_at_a_time() {
    let dir = scratch("concurrent");
    let d = dir.display();
    let claude = fake_claude(
        &dir,
        &format!(
            "mkdir '{d}/run.'$$\nls -d '{d}'/run.* | wc -l >> '{d}/counts'\nsleep 0.3\nrmdir '{d}/run.'$$\ncase \"$1\" in\n  --version) echo '2.1.318 (Claude Code)';;\n  auth) printf '%s' '{{\"loggedIn\":true}}';;\nesac"
        ),
    );
    let binary = claude.to_str().expect("utf-8 path").to_string();
    let env = cli_env(smelt_env(Some(&dir.join("c"))));
    let checks = (0..6).map(|_| status_with(&binary, env.clone(), Duration::from_secs(10)));
    for status in futures_util::future::join_all(checks).await {
        assert!(status.signed_in, "{status:?}");
    }
    let counts = std::fs::read_to_string(dir.join("counts")).expect("the fake counted");
    let most = counts.lines().filter_map(|n| n.trim().parse::<usize>().ok()).max().expect("counts");
    assert!(most <= MAX_STATUS_CHECKS, "{most} CLI processes ran at once");
}

/// Whether process `pid` is gone: no `/proc` entry, or a zombie waiting
/// to be reaped by whoever inherited it.
fn process_gone(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

/// Review 1 L4: a CLI that times out goes with everything it started,
/// not only the process smelt started.
#[tokio::test]
async fn test_a_hung_cli_is_killed_with_what_it_started() {
    let dir = scratch("hung-group");
    let pid_file = dir.join("grandchild.pid");
    let claude = fake_claude(&dir, &format!("sleep 30 &\necho $! > '{}'\nwait", pid_file.display()));
    let status = status_with(claude.to_str().expect("utf-8 path"), cli_env(smelt_env(Some(&dir.join("c")))), Duration::from_millis(500)).await;
    assert!(status.error.as_deref().is_some_and(|e| e.contains("didn't answer")), "{status:?}");
    let pid: u32 = std::fs::read_to_string(&pid_file).expect("the fake started its sleep").trim().parse().expect("a pid");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !process_gone(pid) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let gone = process_gone(pid);
    if !gone {
        // Don't leave it running past a failed run.
        let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
    }
    assert!(gone, "the CLI's own child outlived the timeout");
}
