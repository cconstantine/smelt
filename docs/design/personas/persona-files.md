# Persona files
Part of the [personas design](index.md) (SME-234).

What a persona file is, where it lives in a repo, how smelt finds, trusts, lists and reads one, and how the same file serves Claude Code. The tool rules inside it are in [tool-rules.md](tool-rules.md).

## What our personas need

The format is chosen against what smelt's own seven personas ([docs/personas/](../../personas/README.md)) do, not against a feature list. Each one's effects outside its sandbox, from its persona file and [rules.md](../../personas/rules.md):

| Persona | Instructions it links | Effects outside its sandbox | Pushes | Starts agents |
| -- | -- | -- | -- | -- |
| Project Manager | project-manager.md, rules.md, development-process.md | Linear: moves tickets, comments. GitHub: reads PRs and CI. Runs `scripts/pm-wait-for-board`, which needs the Linear API key | no | Architect, Planner, Developer, Integrator, Facilitator |
| Architect | architect.md, rules.md | Linear: writes the project description, creates tickets | no | no |
| Planner | planner.md, process/plan.md, rules.md | Linear: writes the plan, moves the ticket. Pushes a `docs/plans/sme-N.md` when the plan is long | sometimes | no |
| Developer | developer.md, process/tdd.md, rules.md, rules-commits.md | Pushes the branch, opens the PR, writes Linear; runs the gate (Postgres, a k3s cluster) | yes | Code Reviewer |
| Code Reviewer | code-reviewer.md, close-out.md | Reads everything; writes only a PR review and its own retro note | no | no |
| Integrator | integrator.md, rules.md | Pushes merges of `main`, merges the PR, moves the ticket | yes | Code Reviewer (a retrospective's PR) |
| Facilitator | facilitator.md, close-out.md | Reads transcripts and smelt's API, writes the retrospective ticket | no | a reader agent, optionally |

So a persona file needs: a name and a one-line description (for the listing and for the model choosing one); instructions in Markdown that **link other docs in the repo** rather than copying them (README.md's "a brief points at the rules; it doesn't restate them"); tool rules (the Code Reviewer's "read-only, except a PR review" is the hard case); whether it gets the push credential; optionally its egress, a model and limits for the agents under it. None of them needs a different model today, and none sets a limit: those fields are optional and absent by default.

## The format: Markdown with YAML frontmatter, smelt's settings under one key

```markdown
---
name: code-reviewer
description: Reviews one PR's diff for bugs and security gaps, and changes nothing.
smelt:
  model: claude-opus-4-5
  tools:
    class: read-only
    allow: [github/create_pull_request_review, linear/save_comment]
  credentials: []
---
Take on the Code Reviewer persona: read [code-reviewer.md](../../docs/personas/code-reviewer.md)
and [rules.md](../../docs/personas/rules.md), and follow them.
```

- **Top level: exactly `name`, `description` and `smelt`.** `name` and `description` are required; `smelt` is optional (a persona that is only instructions). Any other top-level key is an error, Claude Code's own (`tools`, `model`, `disallowedTools`…) included (the user's answer to SME-234's question 6), and so is any unknown key at any depth under `smelt` (the user's constraint). `name` is 1–64 characters of `a-z`, `0-9` and `-`, not starting or ending with `-`, and must equal the file's name without `.md` (the Agent Skills rule for `name`): one name per file, so nothing depends on read order. `description` is 1–1024 characters.
- **The body is the instructions,** Markdown, at most 32 KiB (`AGENTS.md`'s cap, `INSTRUCTIONS_MAX_BYTES`, `src/git/server/instructions.rs`). A larger file is an error naming it: `read_instructions_file` returns the first 32 KiB and the file's size, so the parser refuses a size over the cap rather than run truncated instructions. A relative link resolves against the file's own directory. smelt rewrites each relative link to the absolute path in the child's checkout (`/workspace/<dir>/docs/personas/developer.md`) when it loads the persona, so the model can `read_file` it as written; the file on disk keeps its relative links.
- **The `smelt` key** holds every field smelt uses beyond the name, description and body. Its fields arrive with the tickets that use them, and three have: `model` (SME-235), `tools` (SME-241) and `credentials` (SME-242). `egress` (SME-243) and `limits` (SME-245) haven't, and until a ticket adds one, writing it is an error, as for any unknown field.
- **`model`** is a model name (`claude-opus-4-5`), never a provider: providers are each deployment's own configuration and a repo file can't name one. It resolves against the configured providers' models (`provider_models`) in this order: the parent's provider, the default model's provider, then the others by name. No match starts the child on its parent's model, and `start_agent`'s result says so. Absent, the child runs on its parent's model.
- **`tools`** (SME-241) is the persona's tool rules: a class ceiling (`all`, the default, `no-destructive` or `read-only`) plus `allow` and `deny` lists of patterns — a smelt tool name, a whole server as `<server>/*`, or `<server>/<pattern>`, where `*` matches any run of characters holding no `/`. The grammar, the four specificity levels and how a child's chain narrows them are [tool-rules.md](tool-rules.md#the-grammar); `personas::rules` is the code, and every error names the persona file, so a bad rule fails the persona's parse rather than quietly allowing everything. The checked rules ride in `persona_settings` (`{"model": …, "tools": …}`), which is where the child's start reads them to append its entry to the stored chain.
- **`credentials`** (SME-242) lists the credential names this persona runs with — both kinds: the built-in `git-ssh` (the user's git key) and names of the user's named secrets, which land at `/etc/smelt/secrets/<name>` mode 600. Absent or empty is none. A name is 1–64 characters of `A-Za-z0-9`, `-` and `_` (it becomes a file name in a pod); an empty entry or a bad character is a parse error naming the file and the line; a well-formed name the store doesn't hold parses fine and is reported at the agent's start and in its environment section until it exists — the store is consulted at every read, not at the parse. Only `git-ssh` narrows down the parent chain; a named secret is granted by the persona alone ([tool-rules.md](tool-rules.md#credentials)). The names ride in `persona_settings` beside `model` and `tools`, frozen at start.

### Parsing

One server-only module, `src/personas/` (SME-235), is the only code that knows the format. It splits the frontmatter (a first line of exactly `---`, up to the next line of exactly `---`) from the body and deserializes the frontmatter with `serde_norway` (already a dependency, `Cargo.toml`'s `serde_norway = "0.9"`, used for mason's `package.yaml`) into structs that all carry `#[serde(deny_unknown_fields)]`. Errors name the file and the field, with serde's line and column. A file with no frontmatter is an error, not documentation: the persona directory holds only personas.

`serde_norway`, like `serde_yaml` 0.9 before it, resolves plain scalars by YAML 1.2's core schema, so `no` and `on` stay strings; typed fields (a `bool`, an enum of classes) refuse anything else.

### Alternatives

| Option | For | Against |
| -- | -- | -- |
| **Markdown + YAML frontmatter, smelt's fields under `smelt` (chosen)** | Claude Code and opencode already read this shape; instructions stay Markdown that links the repo's docs; one file per persona diffs cleanly | YAML's surprises, contained by typed structs |
| Markdown + frontmatter, smelt's fields at the top level | Shorter | smelt's `tools` and Claude Code's `tools` mean different things, so one file can't serve both, and every Claude Code field would be an error in smelt or have to be accepted |
| TOML file per persona with the instructions in a sibling `.md` (Codex's agent roles, `codex-rs/agent-roles/src/agent_role_config.rs`, `<config>/agents/*.toml` with `deny_unknown_fields`) | TOML has no implicit typing; Codex shows `deny_unknown_fields` on a role file works | Two files per persona; Claude Code can't load it, so SME-247's "usable under Claude Code" needs a generated copy kept in sync |
| TOML frontmatter (`+++`) | No YAML | Claude Code reads only YAML frontmatter |
| One manifest for every persona (`personas.toml`) | One place to look | Every change touches one file; nothing else reads it |

**Why:** it is the only shape one file can serve under both smelt and Claude Code, and keeping smelt's fields under one key is what lets smelt refuse unknown fields without refusing Claude Code's. The user's worry about lock-in is met by the parser being one module and refusing what it doesn't use: a later format change rewrites one module and the files (SME-247's seven), with every file checked by a lint (SME-247's `repo_lints` test).

## Where persona files live

In `.agents/personas/<name>.md`. Persona files are the Markdown files directly in the repo's `.agents/personas/` directory, tracked by git. Not its subdirectories: one flat directory keeps names unique by construction.

`.agents/` follows the cross-client convention the Agent Skills project describes for skills (`<project>/.agents/skills/`, "emerged as a widely-adopted convention for cross-client skill sharing", agentskills.io, "Adding skills support"), so a repo's personas and, later, its skills (SME-37) sit side by side under a name no one tool owns. Personas aren't tied to smelt or to Claude Code, which the project's goal asks for ("nothing is tied to smelt's own process").

**Under Claude Code**, the repo adds a relative symlink, `.claude/agents -> ../.agents/personas`. Spike S1 checked this on Claude Code 2.1.303 (`claude -p` in a scratch repo, the cheapest model):

- the persona under the symlinked directory loaded and ran as a subagent (`spike-reviewer` was listed beside the built-in ones, and delegating to it worked);
- the unknown `smelt` key was ignored, with one debug-log warning: `Agent file …/.claude/agents/spike-reviewer.md has a frontmatter key Claude Code does not recognize or apply: "smelt"`, as the docs say (code.claude.com/docs/en/sub-agents, "Frontmatter reference": "Claude Code ignores a field it doesn't recognize without reporting an error");
- a relative link in the body (`../../docs/process/rules.md`) was followed: the subagent read the linked file and answered from it.

Claude Code identifies a subagent by `name` only, so smelt's "name equals file name" rule costs it nothing. What Claude Code does **not** get from the file: smelt's tool rules, model and credentials. Under Claude Code a persona runs with the session's own permissions and model, as smelt's process does today; SME-247 says so in the process docs. If a later Claude Code refused unknown keys, the symlink would be replaced by generated files (the TOML row above) without touching smelt.

### Alternatives

- **`.claude/agents/` as the real directory:** Claude Code finds it with no symlink, but the repo's process would live in one vendor's directory, against the goal, and a client other than smelt and Claude Code would look elsewhere anyway.
- **`docs/personas/` itself:** smelt's persona docs are prose for people as well as agents, and the directory holds non-personas (`README.md`, `rules.md`). SME-247 makes each `.agents/personas/<name>.md` a short file that links its `docs/personas/<name>.md`.
- **A path set per repo:** nothing needs it yet.

## Found like AGENTS.md: recorded at clone, listed by a tool, gated by trust

Decided by the user (2026-10-18): a conversation without a repo has no personas; a clone records the repo's persona files as it records `agents_files`; the model learns of them through a tool, gated by the same repo trust; they stay scoped to their repo and are named with it.

- **Recorded at clone.** The clone path that runs `list_agents_files` (`git ls-files -z -- AGENTS.md '*/AGENTS.md'`, `src/git/server/instructions.rs`) also runs `git ls-files -z -- '.agents/personas/*.md'`, keeping only files directly in that directory, and stores the relative paths in `conversation_repos.persona_files TEXT[]`, next to `agents_files`. Both the model's `clone_repo` and the user's "Work on a repo" do it, since both go through `record_clone`/`clone_recorded` (`src/git/server/clone.rs`). Paths only, as for `agents_files`: nothing from the files is stored at clone, so an untrusted file's content never reaches the database's model-visible rows.
- **Listed by a tool, `list_personas`,** read-only (its class, [tool-rules.md](tool-rules.md#smelts-own-tools)). It takes no input and returns, for each of the conversation's ready repos, its path and, **only when the repo's remote is trusted** (`repo_trust.trusted`, read under `db::lock_remote_trust` as `request_or_load` does), each persona's `name`, `description` and file path, read from the checkout now. A file that doesn't parse is listed with its error, never skipped (SME-235's "reported, not silently skipped"). For a remote not yet decided, the result says the user hasn't trusted the repo and that `load_instructions` on its `AGENTS.md` asks them; for a declined one, that its personas aren't available. The same check as `test_the_model_never_sees_an_untrusted_agents_md`.
- **Named with the repo.** A persona is always named by its repo and its name together: `start_agent` takes `repo` (the parent's checkout, by its `/workspace/<dir>` path, which is unique in a conversation, `UNIQUE (conversation_id, dir)`) and `persona` (the name). Two repos' `developer`s never meet, and nothing is merged across repos. In text shown to the user or the model, a persona is written `<dir>:<name>` (`smelt:developer`).
- **The system prompt** lists a repo's persona files beside its AGENTS.md files (`turn::system_prompt`'s Repositories line), file names only, ungated, as it lists `agents_files` today: a name is the file's name, not its content. Descriptions, settings and errors come only from `list_personas`, for a trusted remote.

## Read from the child's own checkout

When `start_agent(brief, repo, persona)` names a persona, the persona the child runs is the file **in the child's own checkout, at the child's ref** (user, 2026-10-18), not the parent's copy:

1. `start_agent` checks, before anything is created, that the remote is trusted and, when no `ref` is given (so the child gets the parent's own commit), that `persona` is a file in the parent's checkout **now** (`git ls-files` in the parent's pod, as `list_personas` reads it), not only in the clone-time `persona_files`: a parent that switched branch since its clone may have personas the record lacks. A name that isn't there is refused at once, so a typo costs nothing. `list_personas` re-records `persona_files` from what it reads, so the record and the listing agree. With a `ref`, the persona may exist only there, so it is checked at the child's ref alone (step 3). The child's ref stays the final word.
2. The child is created, its repo cloned at its ref ([checkouts-and-volumes.md](checkouts-and-volumes.md#which-ref-the-childs-clone-is-on)), the remote's trust read again under `lock_remote_trust` (the read itself checks none), and smelt reads `.agents/personas/<name>.md` from the child's pod, as `load_instructions` reads an AGENTS.md (`read_instructions_file`, which refuses a symlink out of the checkout).
3. It parses it. A file missing at that ref, or one that doesn't parse, fails the child's start: the child's conversation shows the error, the child ends `failed`, and the parent gets the error as the child's report ([agents.md](agents.md#how-a-childs-report-and-messages-reach-a-conversation)).
4. The parsed persona is **stored with the child**, as a snapshot: its name, path, commit, body (links rewritten) and its `smelt` settings, in the child's `agent` row ([agents.md](agents.md#the-schema)). The child's system prompt carries the body under a `# Persona` heading, before the Project instructions. A later change to the file on disk, by the child itself or by a pull, changes nothing until a new agent is started: a persona can't widen its own rules by editing its file.

## Skills (SME-37): what they share with personas

A skill (agentskills.io) is a directory with a `SKILL.md`: YAML frontmatter with a required `name` (matching the directory) and `description`, optional `license`, `compatibility`, `metadata` and `allowed-tools`, and a Markdown body, loaded on demand. That is the same shape as a persona file, and both are repo files found at clone and gated by trust.

**They share the mechanism, not the module.** SME-37 should reuse, from SME-235:
- recording at clone: a `skill_dirs TEXT[]` beside `persona_files`, from `git ls-files` under `.agents/skills/*/SKILL.md`;
- the trust gate: listing and loading only for a trusted remote, under `lock_remote_trust`;
- the frontmatter split and the strict `serde_norway` parse, as a helper in `src/personas/` that the skills module calls;
- the naming: `<dir>:<name>`, scoped to the repo.

They differ in use: a skill's body goes into the model's context on demand (like `load_instructions`), a persona's starts a new conversation. A skill's `allowed-tools` would be Claude Code's tool names, so smelt either refuses it or maps it; SME-37 decides, and the strictness rule says refuse until mapped.
