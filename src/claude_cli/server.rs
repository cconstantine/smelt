//! Running the `claude` binary: which one, with what environment, and what
//! its status commands say.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use super::*;

/// The server's variable naming the CLI's config directory
/// (`CLAUDE_CONFIG_DIR`): its sessions and settings. Ephemeral by default;
/// a deployment can mount a volume there (docs/setup/claude-code.md).
pub const CONFIG_DIR_VAR: &str = "SMELT_CLAUDE_CONFIG_DIR";

/// `CONFIG_DIR_VAR`'s default.
pub const DEFAULT_CONFIG_DIR: &str = "/tmp/smelt-claude";

/// The bound on each status command: the same as a provider's model
/// listing, the other "is this there" question the provider page asks.
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(10);

/// What passes from smelt's environment to the CLI, as is. Nothing else
/// does: the CLI prefers `ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_API_KEY`
/// over the subscription, and the dev `.env` sets the first and
/// `ANTHROPIC_BASE_URL`; `DATABASE_URL` and every other secret of smelt's
/// stay smelt's.
const PASSED_THROUGH: [&str; 3] = ["PATH", "HOME", "CLAUDE_CODE_OAUTH_TOKEN"];

/// The environment the CLI runs with, read through `lookup` (smelt's own
/// is `std::env::var_os`): `PASSED_THROUGH` where set, `CLAUDE_CONFIG_DIR`
/// from `CONFIG_DIR_VAR`, and the autoupdater off, since the image pins
/// the version. A process started with it starts from `env_clear()`.
pub fn cli_env(lookup: impl Fn(&str) -> Option<OsString>) -> Vec<(String, OsString)> {
    let mut env: Vec<(String, OsString)> = PASSED_THROUGH
        .iter()
        .filter_map(|name| Some((name.to_string(), lookup(name)?)))
        .collect();
    let config_dir = lookup(CONFIG_DIR_VAR)
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| DEFAULT_CONFIG_DIR.into());
    env.push(("CLAUDE_CONFIG_DIR".to_string(), config_dir));
    env.push(("DISABLE_AUTOUPDATER".to_string(), "1".into()));
    env
}

/// The provider form's binary, checked: a blank one is `DEFAULT_BINARY`;
/// it's a path or a name on `PATH`, never an address or a command line.
pub fn checked_binary(text: &str) -> Result<String, String> {
    let binary = text.trim();
    if binary.is_empty() {
        return Ok(DEFAULT_BINARY.to_string());
    }
    if binary.contains("://") {
        return Err("Claude Code runs a binary, not an address: give its path, or leave it blank for claude on the server's PATH.".to_string());
    }
    if binary.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("The binary is one path, with no spaces or arguments.".to_string());
    }
    Ok(binary.to_string())
}

/// Where `binary` is: a path (it has a `/`) as given, a name searched for
/// on `path`. Either must be an executable file.
fn resolve_binary(binary: &str, path: Option<&OsStr>) -> Result<PathBuf, String> {
    if binary.contains('/') {
        let candidate = PathBuf::from(binary);
        return if is_executable(&candidate) {
            Ok(candidate)
        } else {
            Err(format!("There's no executable at {binary} on the server."))
        };
    }
    path.into_iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(binary))
        .find(|candidate| is_executable(candidate))
        .ok_or_else(|| format!("There's no {binary} on the server's PATH. The server image has one at /opt/claude/bin/claude; elsewhere, install Claude Code or give its path."))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// What `binary` says about itself, run with smelt's own environment
/// through `cli_env`.
pub async fn status(binary: &str) -> ClaudeCliStatus {
    status_with(binary, cli_env(|name| std::env::var_os(name)), STATUS_TIMEOUT).await
}

/// `status` with the CLI's environment and each command's bound given.
/// Never echoes what the CLI printed beyond its version line and the two
/// short words of `auth status` it reads.
pub async fn status_with(binary: &str, env: Vec<(String, OsString)>, timeout: Duration) -> ClaudeCliStatus {
    let failed = |error: String| ClaudeCliStatus { error: Some(error), ..ClaudeCliStatus::default() };
    let path = env.iter().find(|(name, _)| name == "PATH").map(|(_, value)| value.as_os_str());
    let resolved = match resolve_binary(binary, path) {
        Ok(resolved) => resolved,
        Err(e) => return failed(e),
    };
    let config_dir = env.iter().find(|(name, _)| name == "CLAUDE_CONFIG_DIR").map(|(_, value)| PathBuf::from(value));
    if let Some(dir) = &config_dir
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        return failed(format!("Couldn't make the CLI's config directory {}: {e}", dir.display()));
    }
    let version = match run(&resolved, &["--version"], &env, config_dir.as_deref(), timeout).await {
        Ok((true, stdout)) => version_line(&stdout),
        Ok((false, _)) => return failed(format!("{binary} --version failed.")),
        Err(e) => return failed(format!("{binary} --version {e}")),
    };
    let mut status = ClaudeCliStatus { version, ..ClaudeCliStatus::default() };
    match run(&resolved, &["auth", "status"], &env, config_dir.as_deref(), timeout).await {
        Ok((success, stdout)) => match parse_auth_status(&stdout, success) {
            Ok((signed_in, auth_method, plan)) => {
                status.signed_in = signed_in;
                status.auth_method = auth_method;
                status.plan = plan;
            }
            Err(e) => status.error = Some(format!("{binary} auth status {e}")),
        },
        Err(e) => status.error = Some(format!("{binary} auth status {e}")),
    }
    status
}

/// Runs `binary args` from a cleared environment plus `env`, bounded by
/// `timeout`: whether it exited 0, and its stdout. Its stderr is dropped
/// unread, so nothing it says there can reach a page or a log.
async fn run(
    binary: &Path,
    args: &[&str],
    env: &[(String, OsString)],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<(bool, Vec<u8>), String> {
    let mut command = tokio::process::Command::new(binary);
    command
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| format!("didn't answer within {} seconds.", timeout.as_secs_f32()))?
        .map_err(|e| format!("couldn't be run: {e}."))?;
    Ok((output.status.success(), output.stdout))
}

/// `--version`'s first line, if it's a short line of plain text.
fn version_line(stdout: &[u8]) -> Option<String> {
    let line = String::from_utf8_lossy(stdout).lines().next()?.trim().to_string();
    (!line.is_empty() && line.len() <= 80 && line.chars().all(|c| !c.is_control())).then_some(line)
}

/// Reads `auth status`'s JSON (`loggedIn`, `authMethod`,
/// `subscriptionType`), and only that: whether it's signed in, how, and on
/// what plan. Output that isn't JSON is signed out when the CLI exited
/// non-zero (its "not logged in"), else an error that doesn't quote it.
fn parse_auth_status(stdout: &[u8], success: bool) -> Result<(bool, Option<String>, Option<String>), String> {
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(stdout) else {
        return if success { Err("printed something other than JSON.".to_string()) } else { Ok((false, None, None)) };
    };
    let signed_in = body.get("loggedIn").and_then(serde_json::Value::as_bool).unwrap_or(success);
    let word = |key: &str| short_word(body.get(key)?.as_str()?);
    Ok((signed_in, word("authMethod"), word("subscriptionType")))
}

/// `text` if it's a short word (letters, digits, `.`, `_`, `-`): what an
/// auth method or plan name is, and what a token or address isn't.
fn short_word(text: &str) -> Option<String> {
    let ok = !text.is_empty() && text.len() <= 32 && text.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    ok.then(|| text.to_string())
}
