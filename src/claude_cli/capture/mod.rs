//! The capture of the real CLI's output (SME-430's S-checks and
//! fixtures): `smelt claude-cli-capture <out dir> <scenario>`, built only
//! with the `claude-cli-capture` feature and run only by
//! `scripts/claude-cli-capture`. It needs this host's Claude Code sign-in,
//! and each scenario but `sanitize` is one real `claude -p` call, so
//! nothing in the gate runs it.
//!
//! The raw output stays in the out dir, outside the repo; `sanitize`
//! writes the fixtures the fake CLI replays, with every id, date, address
//! and signature replaced.

mod sanitize;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

use super::bridge::{ToolReply, bridge};
use super::process::{Launch, SessionArg, cli_args, new_session_id, turn_env};

type Result<T> = std::result::Result<T, String>;

const SYSTEM_PROMPT: &str = "You are a test assistant for smelt. Follow the user's instructions exactly, and keep every reply to one short line.";

/// What the capture does with each `tools/call` the CLI makes.
#[derive(Clone, Copy)]
enum OnCall {
    /// Answers `value-<key>` after `delay`.
    Answer(Duration),
    /// Kills the CLI with SIGKILL a second after the call arrives (S1).
    Kill,
}

struct Scenario<'a> {
    name: &'a str,
    prompt: &'a str,
    model: &'a str,
    session: SessionArg<'a>,
    tools: bool,
    on_call: OnCall,
    token: Option<String>,
    max_output: u32,
    thinking: bool,
    extra_args: &'a [&'a str],
}

fn base(name: &str) -> Scenario<'_> {
    Scenario {
        name,
        prompt: "",
        model: "claude-opus-5-5",
        session: SessionArg::Ephemeral,
        tools: true,
        on_call: OnCall::Answer(Duration::from_millis(500)),
        token: None,
        max_output: 8_000,
        thinking: true,
        extra_args: &[],
    }
}

/// This host's Claude Code access token, read from its credentials file
/// and never printed or written anywhere: each call gets it as
/// `CLAUDE_CODE_OAUTH_TOKEN` in its environment, with a config directory
/// of the capture's own, so no session or token refresh touches the
/// host's own config.
fn host_token() -> Result<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or("HOME isn't set")?;
    let body = std::fs::read(home.join(".claude/.credentials.json")).map_err(|e| format!("this host's Claude Code credentials: {e}"))?;
    let body: serde_json::Value = serde_json::from_slice(&body).map_err(|e| format!("the credentials aren't JSON: {e}"))?;
    body["claudeAiOauth"]["accessToken"].as_str().map(str::to_string).ok_or_else(|| "the credentials hold no access token".to_string())
}

fn lookup_tool() -> crate::anthropic::ToolDefinition {
    crate::anthropic::ToolDefinition {
        name: "lookup".to_string(),
        description: "Look up the value stored under a key.".to_string(),
        input_schema: serde_json::json!({"type": "object", "properties": {"key": {"type": "string"}}, "required": ["key"]}),
    }
}

fn write(path: &Path, body: impl AsRef<[u8]>) -> Result<()> {
    std::fs::write(path, body).map_err(|e| format!("couldn't write {}: {e}", path.display()))
}

/// Runs one scenario: the CLI's every stdout line, timed, to
/// `<name>.raw.jsonl`; each `tools/call` to `<name>.calls.jsonl`; its exit
/// and the env's names to `<name>.exit.json`.
async fn run(out: &Path, scenario: Scenario<'_>) -> Result<()> {
    let config_dir = out.join("config");
    let work = out.join("work");
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let prompt_file = out.join("system_prompt.txt");
    write(&prompt_file, SYSTEM_PROMPT)?;
    let token = match scenario.token.clone() {
        Some(token) => token,
        None => host_token()?,
    };
    let lookup = |name: &str| -> Option<OsString> {
        match name {
            "CLAUDE_CODE_OAUTH_TOKEN" => Some(token.clone().into()),
            super::CONFIG_DIR_VAR => Some(config_dir.clone().into_os_string()),
            _ => std::env::var_os(name),
        }
    };
    let mut env = turn_env(super::cli_env(lookup), scenario.max_output);
    if !scenario.thinking {
        // What S6 found doesn't turn thinking off.
        env.push(("MAX_THINKING_TOKENS".to_string(), "0".into()));
    }
    let names: Vec<&str> = env.iter().map(|(name, _)| name.as_str()).collect();
    if names.iter().any(|name| name.starts_with("ANTHROPIC_")) {
        return Err(format!("an ANTHROPIC_ variable would reach the CLI: {names:?}"));
    }

    let tools = if scenario.tools { vec![lookup_tool()] } else { Vec::new() };
    let mut run_bridge = bridge().await?.register(&tools);
    let config_file = out.join("mcp.json");
    let _ = std::fs::remove_file(&config_file);
    super::process::write_private(&config_file, run_bridge.mcp_config().to_string().as_bytes()).map_err(|e| e.to_string())?;
    let launch = Launch {
        model: scenario.model,
        effort: None,
        system_prompt_file: &prompt_file,
        mcp_config_file: scenario.tools.then_some(config_file.as_path()),
        session: scenario.session,
    };
    let mut child = tokio::process::Command::new("claude")
        .args(cli_args(&launch))
        .args(scenario.extra_args)
        .env_clear()
        .envs(env.iter().map(|(n, v)| (n, v)))
        .current_dir(&work)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("couldn't run claude: {e}"))?;
    let started = Instant::now();
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err("claude's pipes weren't made".to_string());
    };
    let line = serde_json::json!({"type": "user", "message": {"role": "user", "content": scenario.prompt}});
    stdin.write_all(format!("{line}\n").as_bytes()).await.map_err(|e| e.to_string())?;
    let mut stdout = tokio::io::BufReader::new(stdout).lines();
    let (mut raw, mut calls) = (String::new(), String::new());
    let mut stdin = Some(stdin);
    let mut kill_at: Option<Instant> = None;
    // Kept unanswered until the kill, so the call stays parked.
    let mut held = Vec::new();
    let deadline = started + Duration::from_secs(150);
    loop {
        let wake = kill_at.unwrap_or(deadline).min(deadline);
        tokio::select! {
            line = stdout.next_line() => {
                let Ok(Some(line)) = line else { break };
                let value: serde_json::Value = serde_json::from_str(&line).unwrap_or(serde_json::json!({"unparsed": line}));
                raw.push_str(&serde_json::json!({"t": started.elapsed().as_secs_f64(), "e": value}).to_string());
                raw.push('\n');
                if value["type"] == "result" {
                    stdin = None;
                }
            }
            Some(parked) = run_bridge.calls.recv() => {
                let t = started.elapsed().as_secs_f64();
                calls.push_str(&serde_json::json!({"t": t, "tool_use_id": parked.tool_use_id, "name": parked.name, "arguments": parked.arguments}).to_string());
                calls.push('\n');
                match scenario.on_call {
                    OnCall::Answer(delay) => {
                        let key = parked.arguments["key"].as_str().unwrap_or("none").to_string();
                        tokio::spawn(async move {
                            tokio::time::sleep(delay).await;
                            let _ = parked.reply.send(ToolReply { text: vec![format!("value-{key}")], is_error: false });
                        });
                    }
                    OnCall::Kill => {
                        kill_at = Some(Instant::now() + Duration::from_secs(1));
                        held.push(parked);
                    }
                }
            }
            () = tokio::time::sleep_until(wake.into()) => {
                let _ = child.start_kill();
                kill_at = None;
                if Instant::now() >= deadline { break; }
            }
        }
    }
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    let mut stderr = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = tokio::time::timeout(Duration::from_secs(2), err.read_to_string(&mut stderr)).await;
    }
    let tail: String = stderr.chars().rev().take(2000).collect::<Vec<_>>().into_iter().rev().collect();
    let exit = serde_json::json!({
        "status": format!("{status:?}"),
        "wall_s": started.elapsed().as_secs_f64(),
        "env_names": names,
        "stderr_tail": tail,
    });
    drop(held);
    write(&out.join(format!("{}.raw.jsonl", scenario.name)), raw)?;
    write(&out.join(format!("{}.calls.jsonl", scenario.name)), calls)?;
    write(&out.join(format!("{}.exit.json", scenario.name)), exit.to_string())
}

/// One scenario by name.
async fn scenario(out: &Path, which: &str) -> Result<()> {
    let session_file = out.join("session_id");
    match which {
        // A text reply, and two parallel calls (S2, S7).
        "loop" => {
            let prompt = "Call the lookup tool twice in parallel, in one message: once with key \"a\" and once with key \"b\". Then reply with only the two values, separated by a space.";
            run(out, Scenario { prompt, ..base("loop") }).await
        }
        // A kill while a call is parked (S1, first half).
        "kill" => {
            let id = new_session_id();
            write(&session_file, &id)?;
            let prompt = "Call the lookup tool with key \"hang\", then reply with only the value it returns.";
            run(out, Scenario { prompt, session: SessionArg::New(&id), on_call: OnCall::Kill, ..base("kill") }).await
        }
        // The killed session, resumed, its result as text (S1).
        "resume" => {
            let id = std::fs::read_to_string(&session_file).map_err(|e| format!("the killed session's id: {e}"))?;
            let prompt = "The lookup call above was stopped by smelt; its result: value-hang. Reply with only that value.";
            run(out, Scenario { prompt, session: SessionArg::Resume(id.trim()), ..base("resume") }).await
        }
        // A token Anthropic doesn't know (S5).
        "bad_token" => {
            let token = Some(format!("sk-ant-oat01-{}", "x".repeat(40)));
            run(out, Scenario { prompt: "Reply with only the word ok.", tools: false, token, ..base("bad_token") }).await
        }
        // A model that doesn't exist (S5).
        "bad_model" => {
            run(out, Scenario { prompt: "Reply with only the word ok.", tools: false, model: "claude-nonexistent-0", ..base("bad_model") }).await
        }
        // Thinking off by the env var, and a small reply cap (S6).
        "capped" => {
            let prompt = "Count from 1 to 300 in words, separated by commas.";
            run(out, Scenario { prompt, tools: false, thinking: false, max_output: 40, ..base("capped") }).await
        }
        // Thinking off by the settings flag (S6).
        "thinking_off" => {
            let prompt = "Count from 1 to 300 in words, separated by commas.";
            let extra_args: &[&str] = &["--settings", r#"{"alwaysThinkingEnabled":false}"#];
            run(out, Scenario { prompt, tools: false, max_output: 40, extra_args, ..base("thinking_off") }).await
        }
        "sanitize" => sanitize::all(out, &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/claude_cli/fixtures")),
        other => Err(format!("no capture scenario {other}")),
    }
}

/// `smelt claude-cli-capture <out dir> <scenario>...`: the process's exit
/// code.
pub async fn main(args: Vec<String>) -> i32 {
    let Some((out, scenarios)) = args.split_first() else {
        eprintln!("usage: smelt claude-cli-capture <out dir> <scenario>...");
        return 2;
    };
    let out = PathBuf::from(out);
    for which in scenarios {
        eprintln!("== {which}");
        if let Err(e) = scenario(&out, which).await {
            eprintln!("{which} failed: {e}");
            return 1;
        }
    }
    0
}
