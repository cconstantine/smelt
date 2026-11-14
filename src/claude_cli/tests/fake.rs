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
//! `{"await_interrupt": true}`, `{"stderr": text}`, `{"hang": true}`, `{"exit": code}`,
//! `{"write_session": true}` (its session's file, where the CLI keeps it;
//! `"append"` adds a line to it instead). An
//! interrupt during `read` or `call` ends the call with the CLI's
//! stopped result and an `error_during_execution` `result`, and skips to
//! the next `read`. After the last step, stdin is read to EOF.
//!
//! SME-444's steps: `{"next_call": true}` (the CLI after it echoed the
//! reply's results: `status: requesting`, then, on an interrupt, the
//! expected post-echo lines of `post_echo_interrupt`), and `{"call_mute":
//! [...]}` (as `call`, the answer taken but never echoed). Each process
//! also keeps `transcript.{n}.jsonl`: what the model would read in its
//! session, in order — each user message taken, each `tool_result` echoed
//! (with `is_error`), and each interrupt marker.

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
        "missing" => include_str!("../fixtures/missing.jsonl"),
        // Synthesised, not captured (SME-431): its first line says so.
        "overflow" => include_str!("../fixtures/overflow.jsonl"),
        "window_stop" => include_str!("../fixtures/window_stop.jsonl"),
        // Synthesised, not captured (SME-444): its first line says so.
        "post_echo_interrupt" => include_str!("../fixtures/post_echo_interrupt.jsonl"),
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
        let name = format!("Claude {}", self.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        let provider = crate::db::create_inference_provider(pool, &name, "claude_cli", &binary, "api_key", "", false, Some("anthropic"), true, None)
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

    /// The tools process `n` found on its `--mcp-config` server, by name
    /// (a `list_tools` step).
    pub(crate) fn tools(&self, n: usize) -> Vec<String> {
        self.json_lines(&format!("tools.{n}.jsonl")).iter().filter_map(|t| t["name"].as_str().map(str::to_string)).collect()
    }

    /// What process `n`'s model would read in its session (SME-444).
    pub(crate) fn transcript(&self, n: usize) -> Vec<Value> {
        self.json_lines(&format!("transcript.{n}.jsonl"))
    }

    /// Process `n`'s arguments.
    pub(crate) fn args(&self, n: usize) -> Vec<String> {
        let text = std::fs::read_to_string(self.dir.join(format!("args.of.{n}"))).unwrap_or_default();
        text.split('\0').filter(|a| !a.is_empty()).map(str::to_string).collect()
    }

    /// Process `n`'s `CLAUDE_CODE_MAX_OUTPUT_TOKENS`.
    pub(crate) fn cap(&self, n: usize) -> String {
        std::fs::read_to_string(self.dir.join(format!("cap.{n}"))).unwrap_or_default()
    }

    /// The names in process `n`'s environment, the fake's own two left out.
    pub(crate) fn env_names(&self, n: usize) -> Vec<String> {
        let text = std::fs::read_to_string(self.dir.join(format!("env.{n}"))).unwrap_or_default();
        text.lines().map(str::to_string).collect()
    }

    /// When the fake wrote stamp `name`, or started process `n` (`started.n`).
    pub(crate) fn time_of(&self, file: &str) -> std::time::SystemTime {
        std::fs::metadata(self.dir.join(file)).and_then(|m| m.modified()).expect("the file's time")
    }

    /// The earlier processes still running as process `n` started.
    pub(crate) fn alive_at_start(&self, n: usize) -> Vec<String> {
        let text = std::fs::read_to_string(self.dir.join(format!("alive_at_start.{n}"))).expect("process n started");
        text.lines().map(str::to_string).collect()
    }

    /// The system prompt process `n` was given (`--system-prompt-file`).
    pub(crate) fn prompt(&self, n: usize) -> String {
        std::fs::read_to_string(self.dir.join(format!("prompt.{n}"))).unwrap_or_default()
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
    let n = take_number(&dir);
    // Its pid, and which earlier processes still ran as it started, for a
    // test of two processes in one session (SME-431 L21).
    let _ = std::fs::write(dir.join(format!("pid.{n}")), std::process::id().to_string());
    let alive: Vec<String> = (0..n)
        .filter(|i| std::fs::read_to_string(dir.join(format!("pid.{i}"))).ok().is_some_and(|pid| running(pid.trim())))
        .map(|i| i.to_string())
        .collect();
    let _ = std::fs::write(dir.join(format!("alive_at_start.{n}")), alive.join("\n"));
    let code = runtime.block_on(play(&dir, n));
    // When it ended, for a test of what waits on that (PR #285 review 1 L17).
    let _ = std::fs::write(dir.join(format!("exited.{n}")), "");
    std::process::exit(code);
}

/// Whether process `pid` runs: there, and not a zombie.
fn running(pid: &str) -> bool {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    // The state follows the command's closing parenthesis.
    stat.rsplit_once(") ").and_then(|(_, rest)| rest.chars().next()).is_some_and(|state| !matches!(state, 'Z' | 'X'))
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
                self.log("transcript", &json!({"user": value["message"]["content"]}));
                return Read::User;
            }
            if is_interrupt(&value) {
                return Read::Interrupt;
            }
        }
    }

    /// A tool result the CLI took, echoed on stdout and kept in the
    /// session's transcript.
    async fn echo(&mut self, id: &Value, text: &str, is_error: bool) {
        self.log("transcript", &json!({"tool_result": id, "text": text, "is_error": is_error}));
        let mut block = json!({"type": "tool_result", "tool_use_id": id, "content": [{"type": "text", "text": text}]});
        if is_error {
            block["is_error"] = json!(true);
        }
        self.emit(&json!({"type": "user", "message": {"role": "user", "content": [block]}})).await;
    }

    async fn stopped(&mut self, tool_use_id: Option<&str>) {
        if let Some(id) = tool_use_id {
            let text = "The user doesn't want to proceed with this tool use.";
            self.log("transcript", &json!({"tool_result": id, "text": text, "is_error": true}));
            self.log("transcript", &json!({"marker": "[Request interrupted by user for tool use]"}));
            self.emit(&json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "is_error": true, "content": text}]}})).await;
        } else {
            self.log("transcript", &json!({"marker": "[Request interrupted by user]"}));
        }
        self.emit(&json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "terminal_reason": "aborted_tools"})).await;
    }

    /// The CLI after the reply's results are echoed: its next API call
    /// starting, until an interrupt ends it with the expected lines of
    /// `post_echo_interrupt` (SME-444). Whether it was interrupted.
    async fn next_call(&mut self) -> bool {
        let lines = fixture_lines("post_echo_interrupt");
        let requesting = lines.iter().find(|l| l["subtype"] == "status").cloned().expect("the status line");
        self.emit(&requesting).await;
        loop {
            match self.read().await {
                Read::Interrupt => break,
                Read::Eof => return false,
                Read::User => {}
            }
        }
        self.log("transcript", &json!({"marker": "[Request interrupted by user]"}));
        let after: Vec<Value> = lines.iter().skip_while(|l| l["subtype"] != "status").skip(1).cloned().collect();
        for line in &after {
            self.emit(line).await;
        }
        true
    }

    async fn peer(&mut self) -> rmcp::service::Peer<rmcp::RoleClient> {
        if self.client.is_none() {
            let (url, token) = self.mcp.clone().expect("an --mcp-config");
            self.client = Some(super::bridge::connect(&url, &token).await.expect("the fake connects to the bridge"));
        }
        self.client.as_ref().expect("connected").peer().clone()
    }

    /// Calls `call` through the bridge, unless an interrupt comes first;
    /// `mute`, the answer is taken but never echoed (SME-444).
    async fn call(&mut self, call: &Value, mute: bool) -> bool {
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
        if !mute {
            self.echo(&json!(id), &text, is_error).await;
        }
        true
    }
}

async fn play(dir: &Path, n: usize) -> i32 {
    let args_file = std::env::var_os("SMELT_FAKE_CLAUDE_ARGS").map(PathBuf::from).expect("the args file");
    let _ = std::fs::rename(&args_file, dir.join(format!("args.of.{n}")));
    let args = std::fs::read_to_string(dir.join(format!("args.of.{n}"))).unwrap_or_default();
    let args: Vec<&str> = args.split('\0').collect();
    let names: Vec<String> = std::env::vars_os()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .filter(|name| !name.starts_with("SMELT_FAKE_CLAUDE"))
        .collect();
    std::fs::write(dir.join(format!("env.{n}")), names.join("\n")).expect("log the env");
    // The reply cap it was launched with (SME-431 review 1 M1).
    let cap = std::env::var("CLAUDE_CODE_MAX_OUTPUT_TOKENS").unwrap_or_default();
    std::fs::write(dir.join(format!("cap.{n}")), cap).expect("log the cap");
    let mcp = args.iter().position(|a| *a == "--mcp-config").and_then(|i| args.get(i + 1)).map(|config| {
        let config: Value = serde_json::from_str(&std::fs::read_to_string(config).expect("the MCP config file")).expect("the MCP config");
        let server = &config["mcpServers"]["smelt"];
        let token = server["headers"]["Authorization"].as_str().unwrap_or_default().trim_start_matches("Bearer ").to_string();
        (server["url"].as_str().unwrap_or_default().to_string(), token)
    });
    // Its system prompt, which smelt removes once the run goes.
    if let Some(file) = args.iter().position(|a| *a == "--system-prompt-file").and_then(|i| args.get(i + 1)) {
        let _ = std::fs::copy(file, dir.join(format!("prompt.{n}")));
    }
    let scenario: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("scenario.json")).expect("the scenario")).expect("JSON");
    let steps = scenario[n].as_array().cloned().unwrap_or_default();
    let started_in = std::env::current_dir().ok();
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
        } else if let Some((calls, mute)) = step["call"].as_array().map(|c| (c, false)).or_else(|| step["call_mute"].as_array().map(|c| (c, true))) {
            let mut stopped = false;
            for call in calls {
                if !fake.call(call, mute).await {
                    stopped = true;
                    break;
                }
            }
            stopped
        } else if step.get("next_call").is_some() {
            if !fake.next_call().await {
                return 0;
            }
            false
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
                fake.echo(&id, &text, is_error).await;
            }
            false
        } else if let Some(name) = step["stamp"].as_str() {
            std::fs::write(fake.dir.join(format!("{name}.stamp")), "").expect("a stamp");
            false
        } else if step.get("list_tools").is_some() {
            let peer = fake.peer().await;
            for tool in peer.list_all_tools().await.expect("the bridge lists its tools") {
                fake.log("tools", &json!({"name": tool.name}));
            }
            false
        } else if step.get("write_session").is_some() {
            // Its session's file, as the CLI writes it as it goes (SME-431
            // L22), where smelt's `files` looks for it.
            let id = args.iter().position(|a| *a == "--session-id" || *a == "--resume").and_then(|i| args.get(i + 1)).copied().unwrap_or_default();
            let config = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).expect("a config dir");
            // Its cwd as it started: the CLI knows its paths from then on,
            // so a deleted cwd doesn't stop it writing (PR #291 review 1 L3).
            let cwd = started_in.clone().expect("the cwd");
            let conversation_id: i64 = cwd.file_name().and_then(|n| n.to_str()).and_then(|n| n.parse().ok()).expect("a conversation's work dir");
            let sessions = crate::claude_cli::files::sessions_dir(&config, &crate::claude_cli::files::work_dir(&config, conversation_id));
            std::fs::create_dir_all(&sessions).expect("the sessions dir");
            let file = sessions.join(format!("{id}.jsonl"));
            if step["write_session"] == "append" {
                // As the CLI adds to a resumed session's file.
                use std::io::Write as _;
                let mut appended = std::fs::OpenOptions::new().append(true).create(true).open(&file).expect("the session file");
                appended.write_all(b"{\"appended\":true}\n").expect("an appended line");
            } else {
                std::fs::write(file, "{}\n").expect("the session file");
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
