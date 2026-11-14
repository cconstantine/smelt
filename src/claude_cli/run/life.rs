//! A run's life (SME-430): its spawn, its writes to the CLI's input, and
//! its end in a spawned task. Moved out of `run.rs` whole (SME-520).

use super::*;

impl Run {
    pub(super) async fn spawn(conversation_id: i64, turn_model: &crate::providers::TurnModel, request: &CreateMessageRequest, session: SessionArg<'_>) -> Result<Self, String> {
        let env = super::super::cli_env(|name| std::env::var_os(name));
        let value = |name: &str| env.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
        let config_dir = value("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .ok_or_else(|| format!("Claude Code has no config directory: set {}, or HOME, in the server's environment.", super::super::CONFIG_DIR_VAR))?;
        let work = config_dir.join("work").join(conversation_id.to_string());
        let prompts = config_dir.join("prompts");
        for dir in [&config_dir, &work, &prompts] {
            super::super::private_dir(dir).map_err(|e| format!("Couldn't make Claude Code's directory {} private: {e}", dir.display()))?;
        }
        let binary = super::super::resolve_binary(&turn_model.endpoint.base_url, value("PATH").as_deref())?;
        let prompt_file = prompts.join(format!("{conversation_id}-{}.md", super::super::bridge::random_hex(8)));
        let config_file = (!request.tools.is_empty()).then(|| prompt_file.with_extension("mcp.json"));
        // Made before either file is written: any return from here on, or a
        // drop of the run, removes them (review 1 L6).
        let files = RunFiles(std::iter::once(prompt_file.clone()).chain(config_file.clone()).collect());
        super::super::process::write_private(&prompt_file, request.system.as_deref().unwrap_or_default().as_bytes())
            .map_err(|e| format!("Couldn't write the system prompt for Claude Code: {e}"))?;
        let run_bridge = bridge().await?.register(&request.tools);
        if let Some(config_file) = &config_file {
            super::super::process::write_private(config_file, run_bridge.mcp_config().to_string().as_bytes())
                .map_err(|e| format!("Couldn't write Claude Code's MCP config: {e}"))?;
        }
        let launch = Launch {
            model: &turn_model.model,
            effort: turn_model.effort,
            system_prompt_file: &prompt_file,
            mcp_config_file: config_file.as_deref(),
            session,
        };
        let env = turn_env(env, request.max_tokens);
        // One start at a time: every process shares the config dir, and
        // each writes its .claude.json as it starts (SME-399's L11). Held
        // until this one says its first line, or LAUNCH_HOLD passes.
        let launching = LAUNCH.clone().lock_owned().await;
        let spawned = tokio::process::Command::new(&binary)
            .args(cli_args(&launch))
            .env_clear()
            .envs(env.iter().map(|(name, value)| (name, value)))
            .current_dir(&work)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => return Err(format!("Couldn't run Claude Code ({}): {e}", binary.display())),
        };
        let (Some(stdin), Some(stdout), Some(stderr_pipe)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
            return Err("Claude Code's pipes weren't made.".to_string());
        };
        let stderr = Arc::new(Mutex::new(String::new()));
        tokio::spawn(keep_tail(stderr_pipe, stderr.clone()));
        Ok(Self {
            child,
            stdin: Some(stdin),
            lines: BufReader::new(stdout).lines(),
            stderr,
            bridge: run_bridge,
            state: State::Streaming,
            expected: Vec::new(),
            unparsed: std::collections::HashSet::new(),
            arrived: HashMap::new(),
            anonymous: Vec::new(),
            answers: HashMap::new(),
            synced: synced(&[]),
            rate: None,
            _files: files,
            healthy: true,
            interrupts: 0,
            heard: false,
            gone: false,
            max_tokens: request.max_tokens,
            failure: Failure::Diverged,
            conversation_id,
            has_tools: !request.tools.is_empty(),
            cli_tools: None,
            held: Vec::new(),
            session_id: String::new(),
            resumed: false,
            reply_started: false,
            launching: Launching::hold(launching),
            streaming_ids: std::collections::HashSet::new(),
            answered: std::collections::HashSet::new(),
            delivered: false,
            model: turn_model.model.clone(),
            reported: None,
            learned: false,
            overflow: None,
        })
    }

    pub(super) async fn write_line(&mut self, line: &Value) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("Claude Code's input is closed.")?;
        let mut bytes = line.to_string().into_bytes();
        bytes.push(b'\n');
        // Bounded: a CLI that never reads its input fails only its own call
        // (PR #285 review 1 L18).
        let writing = async {
            stdin.write_all(&bytes).await?;
            stdin.flush().await
        };
        let Ok(written) = tokio::time::timeout(write_timeout(), writing).await else {
            self.healthy = false;
            return Err(format!("Claude Code didn't take its input within {} seconds, so smelt stopped it.", write_timeout().as_secs()));
        };
        written.map_err(|e| {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                self.gone = true;
            }
            format!("Couldn't write to Claude Code: {e}")
        })
    }

    /// Whether the process is still running.
    pub(super) fn alive(&mut self) -> bool {
        !self.gone && matches!(self.child.try_wait(), Ok(None))
    }

    /// Whether the process is known to be gone.
    pub(super) fn exited(&mut self) -> bool {
        self.gone || matches!(self.child.try_wait(), Ok(Some(_)))
    }

    pub(super) async fn write_user(&mut self, text: &str) -> Result<(), String> {
        self.write_line(&serde_json::json!({"type": "user", "message": {"role": "user", "content": text}})).await?;
        self.state = State::Streaming;
        Ok(())
    }

    pub(super) async fn interrupt(&mut self) -> Result<(), String> {
        self.interrupts += 1;
        let id = format!("smelt-interrupt-{}", self.interrupts);
        self.write_line(&serde_json::json!({"type": "control_request", "request_id": id, "request": {"subtype": "interrupt"}})).await
    }

    pub(super) fn stderr_tail(&self) -> String {
        self.stderr.lock().map(|tail| tail.trim().to_string()).unwrap_or_default()
    }

    /// Ends the run in a spawned task: interrupt if it's mid-reply, then
    /// close its input, wait, and kill what's left of its process group.
    pub(super) fn end_in_background(self) {
        self.ending(None);
    }

    /// Ends the run in a spawned task, closing it first with `replies`
    /// when given (SME-444's Closing, `close::settle`): answered, echoed,
    /// then the same interrupt and wind-down as any run.
    pub(super) fn ending(mut self, replies: Option<close::Close>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let conversation_id = self.conversation_id;
        let group = self.child.id().and_then(|id| i32::try_from(id).ok());
        let ending = runtime.spawn(async move {
            let interrupt = match replies {
                Some(replies) => self.settle(replies).await,
                None => self.state != State::Idle,
            };
            if interrupt && self.interrupt().await.is_ok() {
                let _ = tokio::time::timeout(WIND_DOWN, async {
                    while let Ok(event) = self.next_event().await {
                        if event["type"] == "result" {
                            break;
                        }
                    }
                })
                .await;
            }
            // Answered with an error only now, after the interrupt: the CLI
            // then records each as stopped rather than calling again.
            self.arrived.clear();
            self.anonymous.clear();
            self.stdin = None;
            if tokio::time::timeout(WIND_DOWN, self.child.wait()).await.is_err() {
                if let Some(group) = self.child.id().and_then(|id| i32::try_from(id).ok()) {
                    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(group), nix::sys::signal::Signal::SIGKILL);
                }
                let _ = self.child.wait().await;
            }
        });
        let mut endings = endings();
        let mine = endings.entry(conversation_id).or_default();
        mine.retain(|ending| !ending.task.is_finished());
        mine.push(Ending { task: ending, group });
    }
}
