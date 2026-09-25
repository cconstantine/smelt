# Coding session

## What

smelt's purpose is to be a coding agent, not a general chat app with a coding mode: every conversation is a coding session. Most of what that needs has shipped. Each piece has its own completed doc:

- A sandbox pod per conversation (`20260809-k8s-sandbox.md`)
- Persistent terminals in it (`20260812-sandbox-terminal.md`)
- A live panel showing what runs (`20260815-sandbox-visibility.md`)
- File tools with diffs (`20260816-file-tools.md`), plus `glob`/`grep` (`20260922-glob-and-grep.md`)
- Memory and CPU limits with OOM detection (`20260816-sandbox-oom.md`)
- A custom sandbox image and volumes (`20260818-sandbox-native-environment.md`)
- MCP servers, including GitHub (`20260817-mcp-servers.md`)

This file keeps only what's still open.

## Still open

1. **A coding-oriented system prompt.** Normal turns send no `system` prompt at all (only compaction has one). Planned as the `system-prompt` project.
2. **Getting a repo into the sandbox.**
   - The sandbox image (`debian:trixie-slim` plus `sudo`) has no `git` unless the model installs it itself.
   - No git credentials reach the pod. The earlier sketch was a per-session Kubernetes `Secret`, mounted read-only into just that pod and deleted with it; the `park` service account can already manage secrets.
   - Undecided: does the user give a repo URL per conversation, or is smelt scoped to one project per deployment?
   - Related but different: GitHub over MCP gives API-level repo access today, and `mcp-hosted-servers.md` covers a git MCP server next to the sandbox. Neither gives the model a real local clone to build and test in.
3. **Deleting idle pods.** A pod ends only when the model terminates it or the conversation is deleted, so a forgotten conversation keeps its pod running indefinitely.
4. **A time limit per command.** `run_terminal_command` has none; a hung command runs until the model sends it a signal.
5. **Confirmation before destructive actions.** A sandboxed `rm -rf` is still destructive within the session's own files. `clarifying-question-tool.md` mentions this as a motivation but doesn't design it.
6. **Describing smelt as a coding agent.** `state.md` still calls it an "AI chat agent", and its out-of-scope list should be reread with coding as the purpose.

## Deliberately deferred (not smelt code)

- **A stronger runtime than `runc`** (gVisor/Kata `RuntimeClass`): none is installed on `homelab`. Adding one is a cluster-admin task; smelt would then only set `runtimeClassName` on the pod spec.
- **Network egress restriction:** the `park` service account has no rights on `NetworkPolicy` objects, so it needs wider permissions or an admin-managed policy. Not a launch requirement; revisit if the threat model changes.
