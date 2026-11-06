//! Running the `claude` binary: which one, with what environment, and what
//! its status commands say.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use super::*;

/// The server's variable naming the CLI's config directory
/// (`CLAUDE_CONFIG_DIR`): its sessions and settings. A deployment can
/// mount a volume there (docs/setup/claude-code.md).
pub const CONFIG_DIR_VAR: &str = "SMELT_CLAUDE_CONFIG_DIR";

/// `CONFIG_DIR_VAR`'s default, under smelt's `$HOME`: never a shared
/// `/tmp`, where another user could make the directory first and plant
/// settings the CLI would run as smelt's user (review 1 L5).
pub const DEFAULT_CONFIG_SUBDIR: &str = ".local/state/smelt-claude";

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
/// from `CONFIG_DIR_VAR`, else `DEFAULT_CONFIG_SUBDIR` under `HOME` (with
/// neither, none: `status_with` then says so), and the autoupdater off,
/// since the image pins the version. A process started with it starts
/// from `env_clear()`.
pub fn cli_env(lookup: impl Fn(&str) -> Option<OsString>) -> Vec<(String, OsString)> {
    let mut env: Vec<(String, OsString)> = PASSED_THROUGH
        .iter()
        .filter_map(|name| Some((name.to_string(), lookup(name)?)))
        .collect();
    let set = |name: &str| lookup(name).filter(|value| !value.is_empty());
    let config_dir = set(CONFIG_DIR_VAR).or_else(|| set("HOME").map(|home| PathBuf::from(home).join(DEFAULT_CONFIG_SUBDIR).into_os_string()));
    if let Some(config_dir) = config_dir {
        env.push(("CLAUDE_CONFIG_DIR".to_string(), config_dir));
    }
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
    relative_path_refused(binary)?;
    Ok(binary.to_string())
}

/// A path with a `/` must be absolute: a relative one would be checked
/// from smelt's working directory and run from the CLI's, which is its
/// config directory (review 1 L1).
fn relative_path_refused(binary: &str) -> Result<(), String> {
    if binary.contains('/') && !Path::new(binary).is_absolute() {
        return Err(format!("{binary} is a relative path: give the binary's absolute path, or a name on the server's PATH."));
    }
    Ok(())
}

/// Where `binary` is: an absolute path as given, a name searched for on
/// `path`. Either must be an executable file.
fn resolve_binary(binary: &str, path: Option<&OsStr>) -> Result<PathBuf, String> {
    relative_path_refused(binary)?;
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

/// How many status checks run at once (review 1 L3): the status is a GET
/// any page can call, so a loop can't start more CLI processes than this.
pub const MAX_STATUS_CHECKS: usize = 2;

/// The status of provider `id`'s CLI, for its page: refused for a
/// provider of another kind, whose `base_url` is an address, never a
/// binary to run (review 1 L3).
pub async fn provider_status(pool: &sqlx::PgPool, id: i64) -> Result<ClaudeCliStatus, String> {
    let provider = crate::db::get_inference_provider(pool, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("That provider no longer exists.")?;
    if crate::providers::ProviderKind::parse(&provider.kind) != Some(crate::providers::ProviderKind::ClaudeCli) {
        return Err("That provider isn't a Claude Code provider.".to_string());
    }
    Ok(status(&provider.base_url).await)
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
    static CHECKS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_STATUS_CHECKS);
    let failed = |error: String| ClaudeCliStatus { error: Some(error), ..ClaudeCliStatus::default() };
    // Held across both commands; the semaphore is never closed.
    let Ok(_permit) = CHECKS.acquire().await else {
        return failed("The status check is shutting down.".to_string());
    };
    let path = env.iter().find(|(name, _)| name == "PATH").map(|(_, value)| value.as_os_str());
    let resolved = match resolve_binary(binary, path) {
        Ok(resolved) => resolved,
        Err(e) => return failed(e),
    };
    let Some(config_dir) = env.iter().find(|(name, _)| name == "CLAUDE_CONFIG_DIR").map(|(_, value)| PathBuf::from(value)) else {
        return failed(format!("The CLI has no config directory: set {CONFIG_DIR_VAR}, or HOME, in the server's environment."));
    };
    if let Err(e) = private_dir(&config_dir) {
        return failed(format!("Couldn't make the CLI's config directory {} private: {e}", config_dir.display()));
    }
    let config_dir = Some(config_dir);
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

/// Makes `dir`, and any missing parent, mode 0700, and makes an existing
/// `dir` 0700 too: only its owner can, so a directory someone else made
/// is refused rather than used (review 1 L5).
fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Runs `binary args` from a cleared environment plus `env`, bounded by
/// `timeout`: whether it exited 0, and its stdout. Its stderr is dropped
/// unread, so nothing it says there can reach a page or a log. It runs in
/// a process group of its own, which a timeout kills whole, so nothing it
/// started outlives it (review 1 L4).
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
        .process_group(0)
        .kill_on_drop(true);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let child = command.spawn().map_err(|e| format!("couldn't be run: {e}."))?;
    let group = child.id();
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(output) => {
            let output = output.map_err(|e| format!("couldn't be read: {e}."))?;
            Ok((output.status.success(), output.stdout))
        }
        Err(_) => {
            // The group's id stays taken while any member lives, so this
            // can't reach another process even after the leader is reaped.
            if let Some(group) = group.and_then(|id| i32::try_from(id).ok()) {
                let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(group), nix::sys::signal::Signal::SIGKILL);
            }
            Err(format!("didn't answer within {} seconds.", timeout.as_secs_f32()))
        }
    }
}

/// `--version`'s first line, if it's a short line of plain text.
fn version_line(stdout: &[u8]) -> Option<String> {
    let line = String::from_utf8_lossy(stdout).lines().next()?.trim().to_string();
    (!line.is_empty() && line.len() <= 80 && line.chars().all(|c| !c.is_control())).then_some(line)
}

/// Reads `auth status`'s JSON (`loggedIn`, `authMethod`,
/// `subscriptionType`), and only that: whether it's signed in, how, and on
/// what plan. Output that isn't JSON is an error that doesn't quote it,
/// whatever the exit code: an older CLI, a crash or a wrapper says nothing
/// about the sign-in, and "not signed in" would send the user to set a
/// token that may be set already (review 1 L2).
fn parse_auth_status(stdout: &[u8], success: bool) -> Result<(bool, Option<String>, Option<String>), String> {
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(stdout) else {
        return Err("didn't say whether it's signed in: it printed something other than JSON.".to_string());
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
