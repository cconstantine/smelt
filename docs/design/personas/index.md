# Personas and sub-agents: the design

The design the "Personas and sub-agents" project's tickets follow (SME-234, the project's discovery ticket). It settles the file format, tool rules, how agents message each other, a child's checkout and volumes, and the limits, each with the alternatives considered and why. The project's own description in Linear holds the user's words and the overview they agreed; this design builds on them and doesn't repeat them.

| Page | What it decides |
| -- | -- |
| [persona-files.md](persona-files.md) | What our personas need; the file format; where files live; how a clone finds them, the listing tool, the trust gate and naming; reading a persona from the child's checkout; Claude Code; skills (SME-37) |
| [tool-rules.md](tool-rules.md) | Every tool's class; MCP classes and the user's override; the rule grammar and its evaluation; narrowing from parent to child; credentials; egress |
| [agents.md](agents.md) | The schema; the agent tools; how messages and reports enter the turn loop; statuses; Stop, `stop_agent`, deletes and restarts; a state × event table; limits and SME-162; SME-38 |
| [checkouts-and-volumes.md](checkouts-and-volumes.md) | Which ref a child's clone is on; trust carried to the child; the volume tools, schema and deletion; sharing a claim between pods; concurrent writers |

## The decisions on one page

1. **A persona file** is Markdown with YAML frontmatter: `name`, `description` and a `smelt` map at the top level, nothing else; the body is the instructions, linking the repo's docs. One module, `src/personas/`, parses it with `serde_norway` and `deny_unknown_fields`.
2. **It lives at `.agents/personas/<name>.md`;** a `.claude/agents -> ../.agents/personas` symlink makes the same files Claude Code subagents (spike S1). Claude Code ignores the `smelt` key, so it gets the instructions but not smelt's rules.
3. **Found like AGENTS.md:** a clone records `persona_files`; `list_personas` shows them only for a trusted remote; a persona is named with its repo (`<dir>:<name>`); the child's copy, at its own ref, is the one it runs, snapshotted at start.
4. **Every tool has a class,** `read-only`, `writing` or `destructive`; sandbox-local work is read-only; MCP tools from their annotations (none means destructive), overridable per tool by the user.
5. **Rules** are a class ceiling plus `allow` and `deny` patterns (`write_file`, `github/get_*`, `github/*`); the most specific matching level wins, deny wins a tie, and the ceiling decides when nothing matches.
6. **A child never gets more than its parent:** its effective rules, credentials and volumes are its persona's and its parent's together, fixed at start.
7. **Credentials follow the persona:** it lists what it gets (`git-ssh`); without it, smelt clones with the key and removes it before the first turn.
8. **Egress** by NetworkPolicy is feasible on k3s (spike S3), CIDRs only. A pod under narrowed rules must never reach smelt's own API, which takes `curl` without a login, and neither may smelt's own fetchers (`http_request`, `webfetch`, the browser) on its behalf; without that, tool rules are advisory.
9. **Messages and reports** go through a durable queue (`agent_messages`), drained under the recipient's turn lock and woken with `Notice::Wake`, so they survive restarts and several land in one turn. A child reports with an explicit `finish(report)`; a child that stops without one gets its parent a notice.
10. **Statuses** are derived from where they already live, except the lifecycle (`starting`, `active`, `finished`, `stopped`, `failed`), which is stored. The pause after a Stop becomes stored.
11. **Stop** on a conversation stops only it; `stop_agent` stops a child's whole subtree. **Deleting** a conversation deletes its subtree.
12. **A child's clone** is at a `ref` argument, by default the parent's exact commit, which must be on its upstream with a clean tree; unpushed work goes on a volume. Trust is per remote, so the parent's carries over with no card.
13. **Volumes** the model creates are owned by its conversation, passed only down the tree, mounted from a pod's next start, and deleted with their owner's subtree; `ReadWriteOnce` on the single-node cluster is enough (spike S2).
14. **Limits** (agents at once, depth, budget) are counted over the group under a lock on the root; a parent never waits holding a provider slot, so SME-162's limit can't deadlock it.

## Glossary

- **Persona:** a role an agent takes on: instructions, and optionally a model, tool rules, credentials, egress and limits. Only ever used by starting a child agent with it.
- **Persona file:** a persona as a file in a repo, `.agents/personas/<name>.md`.
- **Child agent** (or agent): a conversation started by another with `start_agent`. Its **parent** is the conversation that started it.
- **Group:** a conversation the user started and every agent under it. Its **root** is that conversation.
- **Tool class:** what a tool does outside the agent's own sandbox: read-only, writing or destructive.
- **Rule:** a persona's ceiling, `allow` or `deny` entry. **Effective rules:** a child's own and every ancestor's together (its **rule chain**).

## What each later ticket's Planner must follow

The Architect (or the Project Manager) applies the "scope change" column to the ticket before it is planned.

| Ticket | Its Planner must follow | Scope change |
| -- | -- | -- |
| SME-235 (persona files found and listed) | [persona-files.md](persona-files.md): the format, `.agents/personas/`, `name` equal to the file name, `serde_norway` with `deny_unknown_fields`, `conversation_repos.persona_files`, `list_personas` gated by trust under `lock_remote_trust`, `<dir>:<name>`, the system prompt's listing | Fields here: `name`, `description`, the body, `smelt.model`. The tool is `list_personas`. The frontmatter split is a helper SME-37 can call. |
| SME-236 (child agents) | [agents.md](agents.md): `conversations.parent_id`, the `agents` table, `agent_messages` with its drain and `Notice::Wake`, the startup sweep for undelivered rows, `finish`, the "ended without a report" notice, the `starting` and `failed` states with the key removed on every exit from the start, `stop_agent` over the subtree, deleting a subtree under the root's lock. [checkouts-and-volumes.md](checkouts-and-volumes.md): `ref` and its default, trust required and read under the lock, the first turn after the clone, the persona snapshot | Adds `ref` to `start_agent`, and `finish` and the report queue (the report needs them). Drops "a persona may start agents only if its file says so": until SME-241, only a conversation without a parent is offered `start_agent`. |
| SME-237 (tool classes) | [tool-rules.md](tool-rules.md#classes): the class table, `http_request` by method, the MCP mapping | Classes every tool that exists when it is built, the agent tools and `list_personas` included. The class function takes the call's input, for `http_request`. |
| SME-238 (messages) | [agents.md](agents.md): `message_agent(to, text)`, `ask_parent` on `pending_questions.asked_of`, with every user-facing reader of `pending_questions` filtered by it, the stored pause (`paused_at`), the restart notice for a child cut off mid-turn, the parent told of the user's Stop, the state × event table and its check-then-act notes | Adds the stored pause, which touches every conversation's Stop. |
| SME-239 (volumes) | [checkouts-and-volumes.md](checkouts-and-volumes.md#volumes): the two tools, `agent_volumes` and `conversation_volume_mounts`, mounting from the next pod start, passing under a lock, deletion with the subtree, the sweep, RWO, the guidance | None. |
| SME-240 (class override) | `mcp_tool_classes`, keyed by server id ([tool-rules.md](tool-rules.md#mcp-tools)) | None. |
| SME-241 (rules enforced) | [tool-rules.md](tool-rules.md#the-grammar): the grammar, the four levels, deny wins a tie, offered and called from one function, the rule chain stored on `agents` | Refuses `/` and `*` in new MCP server names; reports patterns that match no configured server. |
| SME-242 (credentials) | [tool-rules.md](tool-rules.md#credentials): `smelt.credentials`, none by default for a persona, `pod_credentials` at all three install sites, clone then remove, the environment note | Named secrets the user keeps in smelt, for the Project Manager's Linear key (agreed by the user, SME-234's question 4). |
| SME-243 (egress) | [tool-rules.md](tool-rules.md#egress): homelab's enforcement checked first; `open`, `none`, CIDR lists; one policy per pod, labelled and deleted with it; the `park` role's `networkpolicies` | Adds "never smelt" (`SANDBOX_SMELT_CIDRS`) for every pod under narrowed rules, and the same refusal plus the persona's `egress` in smelt's own fetchers (`http_request`, `webfetch`, the browser tools); host-name lists are out. |
| SME-244 (group cost) | Group spend by `model_call_usage.group_root_id` ([agents.md](agents.md#limits-sme-245)) | Adds that column (no FK), filled from the `agents` row when a call is recorded. |
| SME-245 (limits) | [agents.md](agents.md#limits-sme-245): what is counted (`starting` and `active` agents), under the root's lock, refusal not waiting, the budget check before each call, the defaults, smaller wins | None. |
| SME-246 (sidebar) | [agents.md](agents.md#statuses): statuses derived as in the table; waiting for the user includes a trust card | A new `AppEvent` for agent changes, or `TurnsChanged`/`QuestionsChanged` reused; its Planner decides. |
| SME-247 (smelt's personas as files) | `.agents/personas/<name>.md`, each a short file linking `docs/personas/<name>.md`; the `.claude/agents` symlink; a `repo_lints` test that parses every one; under Claude Code no rules apply | The Project Manager's Linear key, as SME-242 provides it. |
| SME-248 (the Done-when run) | If SME-243 shrank, the "never smelt" gap is what stays open | None. |
| SME-269 (new conversations in every client) | Nothing here | None. |

## How it was researched

Sources, at the versions read (2026-10-22): smelt at `origin/main` 276293d; Claude Code's docs (code.claude.com/docs/en/sub-agents and /permissions) and CLI 2.1.303; opencode at `0030a705` (github.com/sst/opencode, `packages/core/src/permission.ts`, `tool/plugin/subagent.ts`, `config/plugin/agent.ts`); Codex at `a3d93e1e` (github.com/openai/codex, `codex-rs/agent-roles/`, `codex-rs/core/src/agents_md.rs`, `codex-rs/prompts/src/model_messages/multi_agent.rs`); the Agent Skills specification (agentskills.io/specification); `rmcp` 3.1.2's `ToolAnnotations`.

What the others do, briefly: Claude Code's permission rules check deny, then ask, then allow, with specificity changing nothing ("an allow rule can't carve an exception out of a deny rule"), and its subagent files ignore unknown fields. opencode's rules are last-match-wins wildcards over an action and a resource, merged global then agent, and its agent config keeps unknown keys as options. Codex's agent roles are TOML with `deny_unknown_fields`, and AGENTS.md is read nearest-first. smelt's grammar takes "most specific wins" from the user's brief, which neither Claude Code's order nor opencode's does, and refuses unknown fields as Codex does.

Three throwaway spikes, run from scripts in the ticket's scratch directory and removed afterwards; nothing from them is committed but these findings:
- **S1** (Claude Code): a persona file with a `smelt` key, in `.agents/personas/`, reached through a `.claude/agents` symlink, run with `claude -p` on the cheapest model. Loaded; the key ignored with a debug-log warning; a relative link followed. ([persona-files.md](persona-files.md#where-persona-files-live))
- **S2** (shared claim) and **S3** (egress): a k3s `v1.34.6-k3s1` container of their own in this host's Docker, never the shared clusters. ([checkouts-and-volumes.md](checkouts-and-volumes.md#sharing-a-claim-between-pods), [tool-rules.md](tool-rules.md#egress))
