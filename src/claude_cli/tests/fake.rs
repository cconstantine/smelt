//! The fake Claude Code CLI (SME-430): this test binary, run again as
//! `fake_claude_entry`. A test writes a scenario, one list of steps per
//! process the turn starts, and a `claude` shell script that saves its
//! arguments and execs this binary; the fake then plays the scenario's
//! steps on stdout, calls the bridge's tools as the real CLI does (one at
//! a time, with the `_meta` tool-use id), and logs what it was sent and
//! given. Its event lines have the S-check capture's shapes
//! (`../fixtures/`), several of them replayed from it.
//!
//! Steps: `{"read": true}` (stdin to the next user message; at EOF the
//! fake exits 0), `{"emit": [line, …]}`, `{"fixture": name, "from": a,
//! "to": b}` (its lines `a..b`), `{"call": [{"id", "name", "input"}, …]}`
//! (each through the bridge, one at a time, then its `tool_result` line),
//! `{"await_interrupt": true}`, `{"stderr": text}`, `{"hang": true}`, `{"exit": code}`. An
//! interrupt during `read` or `call` ends the call with the CLI's
//! stopped result and an `error_during_execution` `result`, and skips to
//! the next `read`. After the last step, stdin is read to EOF.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::status::{fake_claude, scratch};

pub(crate) const ENTRY: &str = "claude_cli::tests::fake::fake_claude_entry";

/// A fixture by name.
fn fixture(name: &str) -> &'static str {
    match name {
        "loop" => include_str!("../fixtures/loop.jsonl"),
        "kill" => include_str!("../fixtures/kill.jsonl"),
        "resume" => include_str!("../fixtures/resume.jsonl"),
        "bad_token" => include_str!("../fixtures/bad_token.jsonl"),
        "bad_model" => include_str!("../fixtures/bad_model.jsonl"),
        "capped" => include_str!("../fixtures/capped.jsonl"),
        "thinking_off" => include_str!("../fixtures/thinking_off.jsonl"),
        "pending" => include_str!("../fixtures/pending.jsonl"),
        _ => "",
    }
}

/// A fixture's lines, parsed.
pub(crate) fn fixture_lines(name: &str) -> Vec<Value> {
    fixture(name).lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
}

/// A test's fake: its directory and the `claude` to give the provider.
pub(crate) struct FakeCli {
    pub dir: PathBuf,
    pub binary: PathBuf,
}

impl FakeCli {
    /// A fake whose `n`th process plays `runs[n]`.
    pub(crate) fn new(name: &str, runs: Vec<Vec<Value>>) -> Self {
        let dir = scratch(&format!("fake-{name}"));
        std::fs::write(dir.join("scenario.json"), Value::Array(runs.into_iter().map(Value::Array).collect()).to_string())
            .expect("write the scenario");
        let exe = std::env::current_exe().expect("the test binary");
        let body = format!(
            "args=$(mktemp '{dir}/args.XXXXXX')\nfor a in \"$@\"; do printf '%s\\0' \"$a\"; done > \"$args\"\nexec env SMELT_FAKE_CLAUDE_DIR='{dir}' SMELT_FAKE_CLAUDE_ARGS=\"$args\" '{exe}' --exact {ENTRY} --nocapture --quiet --test-threads=1",
            dir = dir.display(),
            exe = exe.display(),
        );
        let binary = fake_claude(&dir, &body);
        Self { dir, binary }
    }

    /// A Claude Code provider running this fake, picked for `conversation_id`.
    pub(crate) async fn pick_for(&self, pool: &sqlx::PgPool, conversation_id: i64) -> i64 {
        let binary = self.binary.display().to_string();
        let provider = crate::db::create_inference_provider(pool, "Claude", "claude_cli", &binary, "api_key", "", false, Some("anthropic"), true, None)
            .await
            .expect("create the provider");
        crate::db::set_conversation_model(pool, conversation_id, provider.id, "claude-opus-5-5").await.expect("pick its model");
        provider.id
    }

    /// How many processes have started.
    pub(crate) fn started(&self) -> usize {
        (0..).take_while(|i| self.dir.join(format!("started.{i}")).exists()).count()
    }

    fn json_lines(&self, file: &str) -> Vec<Value> {
        std::fs::read_to_string(self.dir.join(file))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// Every line process `n` was sent on stdin.
    pub(crate) fn stdin(&self, n: usize) -> Vec<Value> {
        self.json_lines(&format!("stdin.{n}.jsonl"))
    }

    /// What each of process `n`'s tool calls got from the bridge.
    pub(crate) fn results(&self, n: usize) -> Vec<Value> {
        self.json_lines(&format!("results.{n}.jsonl"))
    }

    /// Process `n`'s arguments.
    pub(crate) fn args(&self, n: usize) -> Vec<String> {
        let text = std::fs::read_to_string(self.dir.join(format!("args.of.{n}"))).unwrap_or_default();
        text.split('\0').filter(|a| !a.is_empty()).map(str::to_string).collect()
    }

    /// The names in process `n`'s environment, the fake's own two left out.
    pub(crate) fn env_names(&self, n: usize) -> Vec<String> {
        let text = std::fs::read_to_string(self.dir.join(format!("env.{n}"))).unwrap_or_default();
        text.lines().map(str::to_string).collect()
    }

    /// Waits up to 10 s for `check` to hold.
    pub(crate) async fn until(&self, what: &str, check: impl Fn(&Self) -> bool) {
        for _ in 0..200 {
            if check(self) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("the fake never got to: {what}");
    }
}

/// A user message on stdin, as smelt writes one.
pub(crate) fn is_user(line: &Value) -> bool {
    line["type"] == "user"
}

/// An interrupt on stdin.
pub(crate) fn is_interrupt(line: &Value) -> bool {
    line["type"] == "control_request" && line["request"]["subtype"] == "interrupt"
}

/// The fake's process: only when the test binary was run as the fake.
#[test]
fn fake_claude_entry() {
    let Some(dir) = std::env::var_os("SMELT_FAKE_CLAUDE_DIR").map(PathBuf::from) else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
    let code = runtime.block_on(play(&dir));
    std::process::exit(code);
}

/// Takes the next process number.
fn take_number(dir: &Path) -> usize {
    (0..)
        .find(|i| std::fs::OpenOptions::new().write(true).create_new(true).open(dir.join(format!("started.{i}"))).is_ok())
        .expect("a number")
}

struct Fake {
    n: usize,
    dir: PathBuf,
    stdin: tokio::io::Lines<BufReader<tokio::io::Stdin>>,
    stdout: tokio::io::Stdout,
    mcp: Option<(String, String)>,
    client: Option<super::bridge::Client>,
    /// Calls started by `call_early`, awaited by `join`.
    early: Vec<tokio::task::JoinHandle<(Value, String, bool)>>,
}

/// What reading stdin came to.
enum Read {
    User,
    Interrupt,
    Eof,
}

impl Fake {
    fn log(&self, file: &str, line: &Value) {
        use std::io::Write;
        let path = self.dir.join(format!("{file}.{}.jsonl", self.n));
        let mut out = std::fs::OpenOptions::new().create(true).append(true).open(path).expect("the log");
        writeln!(out, "{line}").expect("log a line");
    }

    async fn emit(&mut self, line: &Value) {
        self.stdout.write_all(format!("{line}\n").as_bytes()).await.expect("write stdout");
        self.stdout.flush().await.expect("flush stdout");
    }

    async fn read(&mut self) -> Read {
        loop {
            let Ok(Some(line)) = self.stdin.next_line().await else {
                return Read::Eof;
            };
            let value: Value = serde_json::from_str(&line).unwrap_or(json!({"unparsed": line}));
            self.log("stdin", &value);
            if is_user(&value) {
                return Read::User;
            }
            if is_interrupt(&value) {
                return Read::Interrupt;
            }
        }
    }

    async fn stopped(&mut self, tool_use_id: Option<&str>) {
        if let Some(id) = tool_use_id {
            self.emit(&json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "is_error": true, "content": "The user doesn't want to proceed with this tool use."}]}})).await;
        }
        self.emit(&json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "terminal_reason": "aborted_tools"})).await;
    }

    async fn peer(&mut self) -> rmcp::service::Peer<rmcp::RoleClient> {
        if self.client.is_none() {
            let (url, token) = self.mcp.clone().expect("an --mcp-config");
            self.client = Some(super::bridge::connect(&url, &token).await.expect("the fake connects to the bridge"));
        }
        self.client.as_ref().expect("connected").peer().clone()
    }

    /// Calls `call` through the bridge, unless an interrupt comes first.
    async fn call(&mut self, call: &Value) -> bool {
        let (id, name, input) = (call["id"].as_str().map(str::to_string), call["name"].as_str().unwrap_or_default().to_string(), call["input"].clone());
        let short = name.trim_start_matches("mcp__smelt__").to_string();
        let peer = self.peer().await;
        let calling = super::bridge::call(&peer, &short, input, id.as_deref());
        tokio::pin!(calling);
        let answered = loop {
            tokio::select! {
                answered = &mut calling => break Some(answered),
                read = self.read() => match read {
                    Read::Interrupt | Read::Eof => break None,
                    Read::User => {}
                },
            }
        };
        let Some((text, is_error)) = answered else {
            self.stopped(id.as_deref()).await;
            return false;
        };
        self.log("results", &json!({"id": id, "name": name, "text": text, "is_error": is_error}));
        self.emit(&json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": [{"type": "text", "text": text}]}]}})).await;
        true
    }
}

async fn play(dir: &Path) -> i32 {
    let n = take_number(dir);
    let args_file = std::env::var_os("SMELT_FAKE_CLAUDE_ARGS").map(PathBuf::from).expect("the args file");
    let _ = std::fs::rename(&args_file, dir.join(format!("args.of.{n}")));
    let args = std::fs::read_to_string(dir.join(format!("args.of.{n}"))).unwrap_or_default();
    let args: Vec<&str> = args.split('\0').collect();
    let names: Vec<String> = std::env::vars_os()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .filter(|name| !name.starts_with("SMELT_FAKE_CLAUDE"))
        .collect();
    std::fs::write(dir.join(format!("env.{n}")), names.join("\n")).expect("log the env");
    let mcp = args.iter().position(|a| *a == "--mcp-config").and_then(|i| args.get(i + 1)).map(|config| {
        let config: Value = serde_json::from_str(&std::fs::read_to_string(config).expect("the MCP config file")).expect("the MCP config");
        let server = &config["mcpServers"]["smelt"];
        let token = server["headers"]["Authorization"].as_str().unwrap_or_default().trim_start_matches("Bearer ").to_string();
        (server["url"].as_str().unwrap_or_default().to_string(), token)
    });
    let scenario: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("scenario.json")).expect("the scenario")).expect("JSON");
    let steps = scenario[n].as_array().cloned().unwrap_or_default();
    let mut fake = Fake {
        n,
        dir: dir.to_path_buf(),
        stdin: BufReader::new(tokio::io::stdin()).lines(),
        stdout: tokio::io::stdout(),
        mcp,
        client: None,
        early: Vec::new(),
    };
    let mut i = 0;
    while i < steps.len() {
        let step = &steps[i];
        i += 1;
        let interrupted = if step.get("read").is_some() {
            match fake.read().await {
                Read::User => false,
                Read::Eof => return 0,
                Read::Interrupt => {
                    fake.stopped(None).await;
                    true
                }
            }
        } else if let Some(lines) = step["emit"].as_array() {
            for line in lines {
                fake.emit(line).await;
            }
            false
        } else if let Some(name) = step["fixture"].as_str() {
            let lines = fixture_lines(name);
            let from = step["from"].as_u64().unwrap_or(0) as usize;
            let to = step["to"].as_u64().map_or(lines.len(), |to| to as usize);
            for line in &lines[from..to] {
                fake.emit(line).await;
            }
            false
        } else if let Some(calls) = step["call"].as_array() {
            let mut stopped = false;
            for call in calls {
                if !fake.call(call).await {
                    stopped = true;
                    break;
                }
            }
            stopped
        } else if let Some(call) = step.get("call_early") {
            // A call made before the reply's lines are written, as the
            // CLI makes a reply's first call before stdout shows its block
            // (review 2 L12).
            let peer = fake.peer().await;
            let call = call.clone();
            fake.early.push(tokio::spawn(async move {
                let short = call["name"].as_str().unwrap_or_default().trim_start_matches("mcp__smelt__").to_string();
                let (text, is_error) = super::bridge::call(&peer, &short, call["input"].clone(), call["id"].as_str()).await;
                (call["id"].clone(), text, is_error)
            }));
            false
        } else if step.get("join").is_some() {
            for early in std::mem::take(&mut fake.early) {
                let (id, text, is_error) = early.await.expect("the early call");
                fake.log("results", &json!({"id": id, "text": text, "is_error": is_error}));
                fake.emit(&json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": [{"type": "text", "text": text}]}]}})).await;
            }
            false
        } else if let Some(ms) = step["sleep_ms"].as_u64() {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            false
        } else if step.get("await_interrupt").is_some() {
            loop {
                match fake.read().await {
                    Read::Interrupt => break,
                    Read::Eof => return 0,
                    Read::User => {}
                }
            }
            fake.stopped(None).await;
            false
        } else if let Some(text) = step["stderr"].as_str() {
            eprintln!("{text}");
            false
        } else if step.get("hang").is_some() {
            std::future::pending::<()>().await;
            false
        } else if let Some(code) = step["exit"].as_i64() {
            return code as i32;
        } else {
            false
        };
        if interrupted {
            while i < steps.len() && steps[i].get("read").is_none() {
                i += 1;
            }
        }
    }
    loop {
        match fake.read().await {
            Read::Eof => return 0,
            Read::Interrupt => fake.stopped(None).await,
            Read::User => {}
        }
    }
}

// --- Event lines in the capture's shapes ---

/// A `stream_event` line.
pub(crate) fn stream(event: Value) -> Value {
    json!({"type": "stream_event", "event": event, "session_id": "00000000-0000-4000-8000-000000000001"})
}

/// One API call's lines: `message_start` with `input` tokens, `blocks`
/// (each a list of its events' inner JSON, index filled in), then
/// `message_delta` with `stop_reason` and `output` tokens, and
/// `message_stop`.
pub(crate) fn api_call(input: i64, blocks: Vec<Vec<Value>>, stop_reason: &str, output: i64) -> Vec<Value> {
    let mut lines = vec![stream(json!({"type": "message_start", "message": {"id": "msg_fixture", "role": "assistant", "content": [],
        "usage": {"input_tokens": input, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 1}}}))];
    for (index, events) in blocks.into_iter().enumerate() {
        for mut event in events {
            event["index"] = json!(index);
            lines.push(stream(event));
        }
        lines.push(stream(json!({"type": "content_block_stop", "index": index})));
    }
    lines.push(stream(json!({"type": "message_delta", "delta": {"stop_reason": stop_reason}, "usage": {"input_tokens": input, "output_tokens": output}})));
    lines.push(stream(json!({"type": "message_stop"})));
    lines
}

/// A text block's events.
pub(crate) fn text_block(text: &str) -> Vec<Value> {
    vec![
        json!({"type": "content_block_start", "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": text}}),
    ]
}

/// A tool call's events, named as the CLI names smelt's tools.
pub(crate) fn tool_block(id: &str, tool: &str, input: &Value) -> Vec<Value> {
    vec![
        json!({"type": "content_block_start", "content_block": {"type": "tool_use", "id": id, "name": format!("mcp__smelt__{tool}"), "input": {}}}),
        json!({"type": "content_block_delta", "delta": {"type": "input_json_delta", "partial_json": input.to_string()}}),
    ]
}

/// A successful `result` line.
pub(crate) fn result_ok() -> Value {
    json!({"type": "result", "subtype": "success", "is_error": false, "result": "", "stop_reason": "end_turn"})
}

/// A reply of `text`: one API call, then its `result`.
pub(crate) fn text_reply(text: &str) -> Value {
    let mut lines = api_call(10, vec![text_block(text)], "end_turn", 3);
    lines.push(result_ok());
    json!({"emit": lines})
}

/// A reply calling each of `calls` (id, smelt tool, input), then the
/// fake's calls to the bridge, one at a time.
pub(crate) fn tool_reply(calls: &[(&str, &str, Value)]) -> Vec<Value> {
    let blocks = calls.iter().map(|(id, tool, input)| tool_block(id, tool, input)).collect();
    let call: Vec<Value> = calls.iter().map(|(id, tool, input)| json!({"id": id, "name": format!("mcp__smelt__{tool}"), "input": input})).collect();
    vec![json!({"emit": api_call(10, blocks, "tool_use", 5)}), json!({"call": call})]
}
