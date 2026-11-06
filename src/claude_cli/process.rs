//! How a turn's `claude -p` process is started (SME-430): its arguments
//! and its environment. The process itself is `run::TurnCli`.

use std::ffi::OsString;
use std::path::Path;

/// Which session a launch is in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SessionArg<'a> {
    /// No session on disk (`--no-session-persistence`): the process holds
    /// the context, and it dies with it. Only the capture runs so since
    /// turns keep sessions (SME-430's second PR).
    #[cfg_attr(not(feature = "claude-cli-capture"), expect(dead_code, reason = "only the capture's scenarios run without a session"))]
    Ephemeral,
    /// A new session with this id (`--session-id`).
    New(&'a str),
    /// The session with this id, resumed (`--resume`).
    Resume(&'a str),
}

/// One launch's settings.
#[derive(Clone, Debug)]
pub struct Launch<'a> {
    pub model: &'a str,
    pub effort: Option<crate::anthropic::Effort>,
    pub system_prompt_file: &'a Path,
    /// The file holding the `--mcp-config` JSON
    /// (`bridge::RunBridge::mcp_config`), if the run has tools: a file,
    /// never the JSON itself, since it holds the bridge's bearer token and
    /// a command line is readable by every user on the host (review 1 M2).
    pub mcp_config_file: Option<&'a Path>,
    pub session: SessionArg<'a>,
}

/// The CLI's arguments for `launch`: print mode with stream-json both
/// ways, the raw API events included; every built-in tool, setting
/// source, skill and slash command off; only the `smelt` MCP server, its
/// tools allowed without asking; smelt's system prompt as given, re-read
/// on every launch (`--system-prompt-snapshot off`, the planner's probe P5).
pub fn cli_args(launch: &Launch<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--tools",
        "",
        "--strict-mcp-config",
        "--setting-sources",
        "",
        "--disable-slash-commands",
        "--permission-mode",
        "dontAsk",
        "--system-prompt-snapshot",
        "off",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    if let Some(config) = launch.mcp_config_file {
        args.extend(["--mcp-config".into(), config.as_os_str().to_owned()]);
        let allowed = format!("mcp__{}__*", super::bridge::SERVER_NAME);
        args.extend(["--allowedTools".into(), allowed.into()]);
    }
    args.extend(["--system-prompt-file".into(), launch.system_prompt_file.as_os_str().to_owned()]);
    args.extend(["--model".into(), launch.model.into()]);
    if let Some(effort) = launch.effort {
        args.extend(["--effort".into(), effort.as_str().into()]);
    }
    match launch.session {
        SessionArg::Ephemeral => args.push("--no-session-persistence".into()),
        SessionArg::New(id) => args.extend(["--session-id".into(), id.into()]),
        SessionArg::Resume(id) => args.extend(["--resume".into(), id.into()]),
    }
    args
}

/// The most an MCP result may be, in tokens, before the CLI writes it to
/// a file instead: well above smelt's own cap on a tool's output
/// (`MAX_TOOL_RESULT_CHARS`), since with the built-in tools off the model
/// couldn't read such a file.
pub const MAX_MCP_OUTPUT_TOKENS: u32 = 100_000;

/// How long the CLI waits on one tool call, in milliseconds: a day. smelt
/// bounds its own tools; the CLI only waits on them.
pub const MCP_TOOL_TIMEOUT_MS: u32 = 86_400_000;

/// A turn's environment: `base` (`cli_env`, the allowlist) plus the CLI's
/// own settings for this launch. Claude Code's compaction is off
/// (`DISABLE_COMPACT`): smelt's history is the record, and smelt compacts
/// (SME-431). The reply's budget is `max_output_tokens`. Thinking has no
/// switch here: neither `MAX_THINKING_TOKENS=0` nor the
/// `alwaysThinkingEnabled` setting turns it off (S6), so it stays on.
pub fn turn_env(mut base: Vec<(String, OsString)>, max_output_tokens: u32) -> Vec<(String, OsString)> {
    let mut set = |name: &str, value: String| base.push((name.to_string(), value.into()));
    set("DISABLE_COMPACT", "1".to_string());
    set("CLAUDE_CODE_MAX_OUTPUT_TOKENS", max_output_tokens.to_string());
    set("MAX_MCP_OUTPUT_TOKENS", MAX_MCP_OUTPUT_TOKENS.to_string());
    set("MCP_TOOL_TIMEOUT", MCP_TOOL_TIMEOUT_MS.to_string());
    base
}

/// Writes `body` to a new file at `path` that only smelt's user can read
/// (mode 0600), refusing one that's already there.
pub fn write_private(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    file.write_all(body)
}

/// A new random session id: a version 4 UUID, which `--session-id` needs.
pub fn new_session_id() -> String {
    let hex = super::bridge::random_hex(16);
    let mut chars: Vec<char> = hex.chars().collect();
    chars[12] = '4';
    // The variant: one of 8, 9, a, b.
    chars[16] = ['8', '9', 'a', 'b'][chars[16].to_digit(16).map_or(0, |d| (d % 4) as usize)];
    let s: String = chars.into_iter().collect();
    format!("{}-{}-{}-{}-{}", &s[0..8], &s[8..12], &s[12..16], &s[16..20], &s[20..32])
}
