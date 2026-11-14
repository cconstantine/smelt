//! A one-shot CLI call (SME-431): `claude -p` with one user message in
//! and one reply out. smelt's compaction summary goes through it on a
//! Claude Code provider (`turn::call::stream_for_kind`), with no session
//! and no tools; a cache re-seed (SME-523, `claude_cli::reseed`) with the
//! turn's tools and its session resumed unsaved. Either is recorded by the
//! caller's meter like any other model call.
//!
//! The process: started under the start lock and the same environment
//! allowlist as a turn's run, given the request's system prompt as its
//! file and its one message as a stream-json user line, its input then
//! closed. It replies once (`--max-turns 1`), says its `result`, and
//! exits; one that hasn't within `WIND_DOWN` of its `result` is killed,
//! and one whose reply is cut off at its cap is killed at once.

use std::process::Stdio;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::process::{Launch, SessionArg, cli_args, turn_env};
use super::run::{Launching, RunFiles, WIND_DOWN, apply, keep_tail, silence};
use crate::anthropic::stream::{Blocks, ModelError, StreamedTurn, UsageMeter};
use crate::anthropic::{ContentBlock, CreateMessageRequest};

/// What a one-shot call's cap adds to its request's `max_tokens`: the CLI
/// can't turn thinking off (S6), and its thinking shares the cap with the
/// reply (PR #291 review 1 M1). With the summary's 2,048 that's 6,144,
/// and on this kind `compaction_transcript_budget` keeps that plus 4,096
/// clear of the transcript for the instructions and the live state.
pub const THINKING_ROOM: u32 = 4_096;

/// The request's one message, as the text the CLI is sent.
fn message_text(request: &CreateMessageRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// What a `result` with `is_error` comes to: an overflow when it reads as
/// one (`anthropic::overflow::classify`), else the CLI's mapped error.
pub fn result_failure(result: &Value, rate: Option<&super::errors::RateLimit>) -> Option<ModelError> {
    let message = super::errors::result_error(result, rate)?;
    let said = result["result"].as_str().unwrap_or_default();
    match crate::anthropic::overflow::classify(None, said) {
        Some(overflow) => Some(ModelError::Overflow(overflow)),
        None => Some(ModelError::Other(message)),
    }
}

/// How a one-shot call's process is launched: the summary's
/// (`OnceLaunch::summary`), or a cache re-seed's (`claude_cli::reseed`).
pub struct OnceLaunch<'a> {
    pub effort: Option<crate::anthropic::Effort>,
    /// The `--mcp-config` JSON (`bridge::RunBridge::mcp_config`) when the
    /// call carries the turn's tools; written to a private file.
    pub mcp_config: Option<Value>,
    pub session: SessionArg<'a>,
}

impl OnceLaunch<'_> {
    /// The compaction summary's: thinking at its least (M1), since a
    /// summary needs little; no tools, no session.
    pub fn summary() -> Self {
        Self { effort: Some(crate::anthropic::Effort::Low), mcp_config: None, session: SessionArg::Ephemeral }
    }
}

/// One call of `request` (one user message) through the CLI.
pub async fn once(
    conversation_id: i64,
    turn_model: &crate::providers::TurnModel,
    request: &CreateMessageRequest,
    launch: OnceLaunch<'_>,
    on_delta: &mut (dyn FnMut(&str) + Send),
    meter: &UsageMeter,
) -> Result<StreamedTurn, ModelError> {
    let fail = |e: String| ModelError::Other(e);
    let env = super::cli_env(|name| std::env::var_os(name));
    let value = |name: &str| env.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
    let config_dir = value("CLAUDE_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| fail(format!("Claude Code has no config directory: set {}, or HOME, in the server's environment.", super::CONFIG_DIR_VAR)))?;
    let work = super::files::work_dir(&config_dir, conversation_id);
    let prompts = config_dir.join("prompts");
    for dir in [&config_dir, &work, &prompts] {
        super::private_dir(dir).map_err(|e| fail(format!("Couldn't make Claude Code's directory {} private: {e}", dir.display())))?;
    }
    let binary = super::resolve_binary(&turn_model.endpoint.base_url, value("PATH").as_deref()).map_err(fail)?;
    let stem = format!("{conversation_id}-{}-once", super::bridge::random_hex(8));
    let prompt_file = prompts.join(format!("{stem}.md"));
    let config_file = launch.mcp_config.as_ref().map(|_| prompts.join(format!("{stem}-mcp.json")));
    let _files = RunFiles([Some(prompt_file.clone()), config_file.clone()].into_iter().flatten().collect());
    super::process::write_private(&prompt_file, request.system.as_deref().unwrap_or_default().as_bytes())
        .map_err(|e| fail(format!("Couldn't write the system prompt for Claude Code: {e}")))?;
    if let (Some(file), Some(config)) = (&config_file, &launch.mcp_config) {
        super::process::write_private(file, config.to_string().as_bytes())
            .map_err(|e| fail(format!("Couldn't write Claude Code's MCP config: {e}")))?;
    }
    let cli_launch = Launch {
        model: &turn_model.model,
        effort: launch.effort,
        system_prompt_file: &prompt_file,
        mcp_config_file: config_file.as_deref(),
        session: launch.session,
    };
    let mut args = cli_args(&cli_launch);
    args.extend(["--max-turns".into(), "1".into()]);
    let launching = Launching::take().await;
    let mut child = tokio::process::Command::new(&binary)
        .args(args)
        .env_clear()
        .envs(turn_env(env, request.max_tokens.saturating_add(THINKING_ROOM)))
        .current_dir(&work)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| fail(format!("Couldn't run Claude Code ({}): {e}", binary.display())))?;
    let (Some(mut stdin), Some(stdout), Some(stderr_pipe)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
        return Err(fail("Claude Code's pipes weren't made.".to_string()));
    };
    let stderr = Arc::new(Mutex::new(String::new()));
    tokio::spawn(keep_tail(stderr_pipe, stderr.clone()));
    let line = serde_json::json!({"type": "user", "message": {"role": "user", "content": message_text(request)}});
    let mut bytes = line.to_string().into_bytes();
    bytes.push(b'\n');
    let written = tokio::time::timeout(super::run::write_timeout(), async {
        stdin.write_all(&bytes).await?;
        stdin.flush().await
    })
    .await;
    if !matches!(written, Ok(Ok(()))) {
        return Err(fail("Couldn't give Claude Code the summary's request.".to_string()));
    }
    // One message, then no more: the CLI answers it and exits.
    drop(stdin);
    let mut lines = BufReader::new(stdout).lines();
    let replied = read(conversation_id, &mut lines, &launching, on_delta, meter).await;
    let kill = |child: &tokio::process::Child| {
        if let Some(group) = child.id().and_then(|id| i32::try_from(id).ok()) {
            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(group), nix::sys::signal::Signal::SIGKILL);
        }
    };
    // A reply cut off at its cap is killed at once: the CLI would carry on
    // by itself (S6), unmetered, and its input is already closed, so no
    // interrupt can reach it (PR #291 review 1 L1).
    if replied.as_ref().is_ok_and(|turn| crate::anthropic::stream::is_cut_off(&turn.stop_reason)) {
        kill(&child);
    }
    // Done with it however the reply went: it's given `WIND_DOWN` to exit.
    if tokio::time::timeout(WIND_DOWN, child.wait()).await.is_err() {
        kill(&child);
        let _ = child.wait().await;
    }
    replied.map_err(|e| match e {
        ModelError::Other(text) if text.is_empty() => {
            let tail = stderr.lock().map(|t| super::errors::scrubbed_line(t.trim())).unwrap_or_default();
            let said = if tail.is_empty() { String::new() } else { format!(" It said: {tail}") };
            ModelError::Other(format!("Claude Code exited before it finished.{said}"))
        }
        e => e,
    })
}

/// Reads the one reply: its stream events through `anthropic::stream`'s
/// block assembler, then its `result`, within `WIND_DOWN`. An error that
/// is the process going is `Other("")`, for `once` to quote its stderr.
async fn read(
    conversation_id: i64,
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    launching: &Launching,
    on_delta: &mut (dyn FnMut(&str) + Send),
    meter: &UsageMeter,
) -> Result<StreamedTurn, ModelError> {
    let mut blocks = Blocks::default();
    let mut stop_reason = String::new();
    let mut rate = None;
    let mut next = async || -> Result<Value, ModelError> {
        loop {
            let line = match tokio::time::timeout(silence(), lines.next_line()).await {
                Err(_) => return Err(ModelError::Other(format!("Claude Code said nothing for {} seconds, so smelt stopped it.", silence().as_secs()))),
                Ok(Ok(Some(line))) => line,
                Ok(_) => return Err(ModelError::Other(String::new())),
            };
            if let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(&line) {
                launching.release();
                return Ok(value);
            }
        }
    };
    loop {
        let event = next().await?;
        match event["type"].as_str() {
            Some("stream_event") => {
                let inner = &event["event"];
                if inner["type"] == "message_stop" {
                    break;
                }
                apply(&mut blocks, &mut stop_reason, inner, on_delta, meter).map_err(ModelError::Other)?;
            }
            Some("rate_limit_event") => {
                super::usage::seen(conversation_id, &event);
                rate = Some(super::errors::RateLimit::from_event(&event));
            }
            Some("result") => {
                return Err(result_failure(&event, rate.as_ref()).unwrap_or_else(|| ModelError::Other("Claude Code ended the reply before it finished.".to_string())));
            }
            _ => {}
        }
    }
    let (content, cut_off_call) = blocks.finish(&stop_reason).map_err(ModelError::Other)?;
    // Cut off: no `result` to wait for, since `once` kills it now (L1).
    if crate::anthropic::stream::is_cut_off(&stop_reason) {
        return Ok(StreamedTurn { content, stop_reason, usage: meter.usage(), cut_off_call });
    }
    // Its `result`, so the process is done; a reply already in hand stands.
    let _ = tokio::time::timeout(WIND_DOWN, async {
        while let Ok(event) = next().await {
            if event["type"] == "result" {
                break;
            }
        }
    })
    .await;
    Ok(StreamedTurn { content, stop_reason, usage: meter.usage(), cut_off_call })
}
