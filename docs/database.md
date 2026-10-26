# Database

Postgres via sqlx. `db.rs`'s CRUD functions take `pool: &PgPool` as an explicit parameter rather than reaching into a global — that's what lets `#[sqlx::test]` (see [testing.md](testing.md#database-tests)) hand each test its own isolated database.

A process-wide `OnceLock<PgPool>` still exists for *production* wiring, so the pool is opened once, at startup:

```rust
// src/db/mod.rs
static POOL: OnceLock<PgPool> = OnceLock::new();

pub async fn init() -> &'static PgPool { /* connects (with retry), sets POOL */ }
pub fn get() -> &'static PgPool { /* panics if init() hasn't run yet */ }
```

`main.rs` calls `db::init()` once at startup, before serving, then runs `sqlx::migrate!()` against it. `init()` panics if `DATABASE_URL` isn't set or doesn't parse, opens a pool of at most 5 connections, and retries a failed connect up to 10 times, 500 ms apart (Postgres may still be starting), before panicking.

Server functions in `src/api/` (`chat.rs`, `git.rs`, `language_servers.rs`, `mcp.rs`, `pods.rs`, `sandbox_volumes.rs`) call the `db.rs` functions directly, passing `db::get()` as the pool argument. Server code outside `api/` (`sandbox/`, `git.rs`, `mcp_oauth.rs`, `anthropic/tools.rs`, `lsp/`, ...) doesn't reach for `db::get()` itself: it takes a `&PgPool` parameter, passed down from a server function or from `main.rs`.

## Query pattern

`INSERT ... RETURNING *` mapped onto the model struct via `sqlx::query_as::<_, T>`, so a create returns the exact row (including DB-assigned `id`/timestamps) in one round trip. Placeholders are Postgres-style `$1, $2, ...`:

```rust
pub async fn create_conversation(pool: &PgPool) -> Result<Conversation, sqlx::Error> {
    sqlx::query_as::<_, Conversation>("INSERT INTO conversations (title) VALUES ($1) RETURNING *")
        .bind(DEFAULT_TITLE)
        .fetch_one(pool)
        .await
}
```

A plain `DELETE` needs no `RETURNING`/`FromRow` mapping at all:

```rust
pub async fn delete_conversation(pool: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM conversations WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
```

Deleting an id that doesn't exist is not an error — it just affects zero rows. Deleting a conversation cascades through everything hanging off it (see [Deleting a conversation](#deleting-a-conversation)).

Other patterns in `db.rs`:

- **Upserts** use `INSERT ... ON CONFLICT`: `DO UPDATE` for last-known-only rows (`conversation_context_usage`, `conversation_todos`, `git_settings`, `repo_trust`, `loaded_instructions`, `instruction_requests`), `DO NOTHING` where an existing row wins (`sandbox_pod_previews`, `ensure_mcp_server`).
- **Single values** (an `EXISTS`, one column) use `sqlx::query_scalar`.
- **JSONB columns** go through `sqlx::types::Json<T>`: bound as `.bind(sqlx::types::Json(value))`, read back as a `Json<T>` field or scalar and unwrapped (`.0`).
- **Soft delete**: `sandbox_pods` and `sandbox_terminals` are never deleted by smelt; terminating one sets `terminated_at = now()`, so the command history under it survives. "Live" means `terminated_at IS NULL`.
- **Private row struct**: `language_servers` is read through a private `LanguageServerRow` (JSONB fields as `Json<T>`) converted into `models::LanguageServer` with `From`. Its queries name their columns via the `LANGUAGE_SERVER_COLUMNS` constant rather than `RETURNING *`/`SELECT *`, so the column list matches the struct.

## FromRow

`Conversation`/`Message` in `models.rs`, and `anthropic::TokenUsage`, derive `sqlx::FromRow` gated as `#[cfg_attr(feature = "server", derive(sqlx::FromRow))]`, since they're compiled into the web build too and the derive pulls in sqlx types not present there. The row structs defined in `db.rs` itself (`SandboxPod`, `SandboxVolume`, `McpServerConfig`, `ConversationRepo`, ...) derive it ungated: the `db` module is only compiled with the `server` feature. `models::LanguageServer` has no `FromRow`; see the private row struct above.

## Errors

`db.rs` functions return `Result<T, sqlx::Error>` directly. Server functions convert with `.map_err(ServerFnError::new)` at the boundary — there's no separate hand-rolled `ApiError` type, since `ServerFnError` (from `dioxus::prelude`) already carries a message through to the client and renders sensibly via `{err}` in the UI. If a specific sqlx error needs distinct client-facing handling, match on it before converting (`e.as_database_error().is_some_and(|d| d.is_unique_violation())`).

Three unique violations are mapped that way today:

- `language_servers.name`: `lsp/config.rs` returns "A language server named … already exists."
- `ssh_keys_only_one`: `git.rs` returns the "There's already an SSH key" refusal when another request added a key meanwhile.
- `sandbox_pods_one_live_per_conversation`: `sandbox` returns `SandboxError::PodAlreadyExists`.

Any other violation (e.g. a duplicate `mcp_servers.name` or `sandbox_volumes.name`) reaches the browser as the raw Postgres message (`duplicate key value violates unique constraint "..."`).

## Migrations

`migrations/*.sql`, applied in filename order by `sqlx::migrate!()` at startup. Never edit a migration once it has been applied anywhere: sqlx stores each applied migration's checksum and refuses to start against a database whose recorded checksum no longer matches the file. The schema changes by adding a new migration.

## Schema

The tables after every migration has run. All `id`s are `BIGINT GENERATED ALWAYS AS IDENTITY` and all timestamps are `TIMESTAMP` (no time zone) defaulting to `now()`, unless noted.

### Conversations

| Table | What it holds | Notes |
|---|---|---|
| `conversations` | `id`, `title`, `created_at`, `updated_at`, `provider_id`, `model`, `last_turn_model_key`, `model_changed_at_message_id`, `parent_id`, `deleting_at`, `paused_at` | `paused_at` (SME-238): when the user's Stop (or a parent's `stop_agent`) paused it, null once the user writes or answers; it mirrors the in-memory pause (`turn::store_pause`, re-written until the two agree) so a restart keeps it (`turn::restore_pauses` at startup, and the startup sweep skips paused rows). `parent_id` (SME-236): the conversation that started this one as its agent, null for the user's own; FK → `conversations` cascade, so deleting a conversation deletes its subtree, with a partial index on it. `deleting_at` (SME-236): set on a whole subtree by a delete under the group root's row lock, so a `start_agent` racing the delete is refused. `provider_id`/`model`: the conversation's model, both null until its first turn takes the default (a `CHECK` keeps them together; FK → `inference_providers` `RESTRICT`, cleared by `delete_inference_provider`). `last_turn_model_key`: the backend (provider, base URL, model) its last turn ran on; `model_changed_at_message_id`: its last message when a turn started on a different one, so thinking signed for the old one isn't replayed. Both set at turn start, under the turn lock (SME-72). `title`'s column default is `'New Conversation'`, but `db::create_conversation` always binds `DEFAULT_TITLE` (`"New conversation"`). Auto-titling from the first user message matches either spelling, case-insensitively. |
| `messages` | `conversation_id`, `role`, `content` (JSON-serialized content blocks as `TEXT`, see [models.md](models.md)) | `role` `CHECK`ed to `user`/`assistant`. FK → `conversations` cascade. |
| `conversation_context_usage` | The last turn's four token counts | PK is `conversation_id` (one row per conversation). FK → `conversations` cascade. |
| `model_call_usage` | One row per model call attempt (SME-106) that got as far as `message_start` or streamed anything, a stopped or failed one included (SME-178): `provider_id`, `model`, `kind` (`turn`/`compaction`), the four token counts, `cost_usd`, `group_root_id` (SME-244), `created_at` | Append-only. Written by `db::record_model_call_if_current` (through `turn::CallRecorder`), which for a finished turn's call also upserts `conversation_context_usage` in the same transaction, unless a Stop dropped the turn while its record was on its way (checked holding the conversation's row, which `clear_conversation_usage` takes too, so a late record can't overwrite a newer turn's usage or undo a compaction's clear); a compaction's isn't the last-known usage, nor is a stopped or failed call's, recorded with what its stream had reported and its output estimated from the bytes streamed when no `message_delta` gave it, its input from the request's size (`estimate_request_tokens`) when `message_start` reported no input at all, as LiteLLM's never does. Unfinished rows aren't marked. The conversation and the provider are each taken through a locking subselect (`FOR NO KEY UPDATE` and `FOR KEY SHARE`), so one deleted before or while the call is recorded gives null rather than a foreign key error. `cost_usd` is null without a price and never recomputed. FK → `conversations` and → `inference_providers` both `SET NULL`: a deleted conversation's or provider's calls stay in the spend they were billed for (conversations since B50, SME-122; before it they cascaded, and "Last 30 days" lost them). `db::get_conversation_spend` sums a conversation's rows for the context detail view, and `db::model_spend_since` each provider's models' rows for the providers page's last 30 days (a deleted provider's under none). `group_root_id` (SME-244): the cost group the call belongs to — the conversation's `agents.root_id` for an agent, its own id for one without a parent — written by the same INSERT; its fallback there is the `conversations` subselect, not the bare id, so a record landing after its conversation row is gone gets NULL with `conversation_id` and joins no group, as it joins no conversation's spend today. No FK by design: `conversation_id` goes `SET NULL` but this id stays, so a deleted agent's calls keep counting in its group's number. The migration backfilled existing rows the same way (agents' rows through `agents.root_id`, every other conversation's to itself, an already-deleted conversation's none) and indexed the column. `db::get_group_spend` sums a group's rows by it, and `db::get_group_root` names the group and its title, for the context detail view's group line. |
| `price_catalog` | The last models.dev price catalog fetched (SME-106): `fetched_at`, `providers` (JSONB, `pricing::Providers`) | Single row, like `git_settings`. Written only by `pricing::CatalogStore::refresh` (startup, then hourly); read once at startup so a restart without network has prices. |
| `conversation_todos` | `items` JSONB, the todo list | PK is `conversation_id`. FK → `conversations` cascade. |
| `agents` | One row per child agent (SME-236): `conversation_id` (PK), `root_id` (the group's root; a nested agent's is its group's root from `create_child`, SME-241), `depth` (1 for the first agent, parent's + 1 below that, `start_agent` refusing past `SMELT_AGENT_DEPTH`, default 5 — SME-241; per-persona limits wait on SME-245), `brief`, `persona_name`, `persona_path`, `persona_commit`, `persona_body` (its relative links made absolute), `persona_settings` JSONB (`{"model": …, "tools": …}`, SME-241), `rule_chain` JSONB (SME-241), `state`, `created_at`, `state_changed_at`, `started_at` | `state` `CHECK`ed to `starting`/`active`/`finished`/`stopped`/`failed`; every write is one `UPDATE … WHERE state = <expected>`. The persona columns are the snapshot taken when the start completes (null without one; `persona_name` is set at `start_agent`). `started_at` is set by the start's done step: a child stopped while starting keeps it null and can never become `active`. `rule_chain` is the conversation's effective tool rules (SME-241): a JSONB array of rule entries — the parent's chain, as `create_child` read it, then the persona's own `smelt.tools` entry appended by `complete_start` — oldest first, evaluated as a conjunction over entries (`src/personas/rules.rs`). NULL is no entries: allow-all, what a child of an unruled parent gets and what every pre-SME-241 row has. Written exactly twice (create, complete) and never again, so a persona-file edit or a server rename changes nothing for a live agent; the offer path and the call path each read it per call, and an absent row, NULL or unreadable JSON falls open with a warning (the column is smelt-written by typed serde; failing closed would strand every tool in the conversation). SME-242 adds `credentials`. FKs `conversation_id` and `root_id` → `conversations` cascade. |
| `agent_messages` | The queue of reports and notices to a conversation (SME-236): `to_conversation_id`, `from_conversation_id`, `from_label` (`agent #12 (smelt:developer)`), `kind`, `body`, `created_at`, `delivered_at`, `delivered_message_id` | `kind` `CHECK`ed to `report`/`notice`, and (SME-238) `message` (`message_agent`) and `question` (a child's `ask_parent`, queued to its parent). Each row becomes one `user` message in its recipient, saved and marked delivered in one transaction under the recipient's turn lock (`db::deliver_agent_message`), and linked by `delivered_message_id` (FK → `messages` `SET NULL`). Partial index `agent_messages_undelivered` on `(to_conversation_id, id) WHERE delivered_at IS NULL`. FK `to_conversation_id` → `conversations` cascade; `from_conversation_id` `SET NULL`, so a deleted sender's rows still arrive under their kept `from_label`. Every transaction that writes `agents` or `agent_messages` takes the remote's trust lock first (if any), then `conversations` rows in ascending id (`db::lock_conversations`), then the rest. |
| `pending_questions` | `tool_use_id`, `questions` JSONB (the model's `ask_user` input), `answer` JSONB (null while waiting), `asked_of` | PK is `conversation_id`: at most one per conversation. FK → `conversations` cascade. The card's answer is written once (`WHERE answer IS NULL`); the next turn deletes the row in the same transaction that saves the call's result (SME-34). `asked_of` (SME-238) is `user` (the default; `ask_user`) or `parent` (a child's `ask_parent`, answered by its parent's next `message_agent` once the question's `agent_messages` row, which shares its `created_at`, is delivered, written as `[{"selected": [], "other": <text>}]`); every reader that means "the user is asked" filters on `user`: the sidebar's waiting list, the card's snapshot and its answer (in the `UPDATE` too), and `list_agents`' "waiting for the user". |

### Sandbox

| Table | What it holds | Notes |
|---|---|---|
| `sandbox_pods` | A conversation's pod: `conversation_id`, `terminated_at` | Soft delete. Partial unique index `sandbox_pods_one_live_per_conversation`: at most one live pod per conversation. FK → `conversations` cascade. |
| `sandbox_terminals` | A terminal in a pod: `pod_id`, `terminated_at` | Soft delete. FK → `sandbox_pods` cascade. |
| `terminal_commands` | A command run in a terminal: `conversation_id`, `terminal_id`, `command_id` (text), `command`, `status`, `exit_code`, `notified_at`, `finished_at`, `idle_notice_secs`, `touched_at`, `idle_notice_at` | `command_id` unique. `status` `CHECK`ed to `running`/`finished`/`lost`. FKs → `conversations` and `sandbox_terminals`, both cascade. Idle notices (SME-79): `idle_notice_secs` is the command's own notice period, stored when it starts — the deployment default (`SANDBOX_IDLE_NOTICE_MINUTES`, 300 s) or `run_terminal_command`'s `idle_notice_seconds` — `CHECK`ed `>= 0` with `0` meaning never, so a running command keeps the interval it started with when the default later changes. `touched_at` is the last moment the model touched the command (checked its status, read its output, signalled it, re-timed its notice — output alone doesn't move it); `NULL` means never, so the clock counts from `created_at`. `idle_notice_at` is when smelt last woke the conversation, so the clock restarts after every notice and there is no cap on how many a command gets. Due means one interval past the latest of those three, in one SQL fragment (`db::IDLE_NOTICE_DUE`) so the watch and the drain can't disagree; the partial index `idx_terminal_commands_running_idle` (`conversation_id`, `created_at` where `running` and `idle_notice_secs > 0`) backs the watch's pass. smelt never stops a command on this clock. |
| `terminal_events` | A command's output: `command_id`, `stream`, `seq`, `data` | `stream` `CHECK`ed to `stdout`/`stderr`. FK → `terminal_commands(command_id)` cascade. |
| `sandbox_pod_previews` | Ports shared as previews: `pod_id`, `host` (`''` is the pod's localhost), `port` | PK `(pod_id, host, port)`; `port` `CHECK`ed to 1–65535. FK → `sandbox_pods` cascade. |
| `sandbox_volumes` | Volumes mounted into every pod: `name`, `mount_path` | `name` unique. No FKs. |

### Git

| Table | What it holds | Notes |
|---|---|---|
| `conversation_repos` | A repo a conversation works on: `url`, `remote_key`, `branch`, `dir`, `status`, `error`, `checked_out_branch`, `commit_sha`, `agents_files` (`TEXT[]`; since SME-271 a path holding a control, line-separator or bidi character, or one Unicode marks default-ignorable (SME-276), isn't recorded, and the ready row's `error` says how many were left out), `attempt`, `persona_files` (`TEXT[]`, SME-235: the tracked files directly in `.agents/personas/` named as a persona's can be (`<name>.md`), paths only; written with `agents_files` when a clone finishes, emptied on a retry, and re-recorded from the checkout by each `list_personas` call, on a ready row of the attempt it read only) | `UNIQUE (conversation_id, dir)`. `status` `CHECK`ed to `cloning`/`ready`/`failed`. Only a `failed` row is retried, which increments `attempt`; ready/failed are written only for the current attempt while `cloning` (SME-86). FK → `conversations` cascade. |
| `loaded_instructions` | An AGENTS.md the model loaded: `path`, `content`, `file_bytes`, `hash`, `commit_sha`, `loaded_at` | `UNIQUE (repo_id, path)`. FKs → `conversations` and `conversation_repos`, both cascade. |
| `instruction_requests` | An AGENTS.md load waiting on the user's trust decision; same columns as `loaded_instructions`. A row at the sentinel path `''` (empty content, its sha256) is a card about the repo itself — no file — asked by a refused `start_agent` (SME-279) | `UNIQUE (repo_id, path)`. FKs → `conversations` and `conversation_repos`, both cascade. |
| `repo_trust` | Whether a remote's AGENTS.md is trusted: `remote_key`, `trusted`, `decided_at` | PK is `remote_key`. No row: not asked yet. No FKs, so it outlives conversations. A decision, a load and "Work on a repo" for a remote take `pg_advisory_xact_lock(hashtextextended('repo_trust:' \|\| remote_key, 0))` first (`db::lock_remote_trust`, SME-181, SME-224), so every write of a remote's trust and of its cards holds it, except two: the Git page's Forget (one `DELETE` of the trust row) and a conversation delete's `ON DELETE CASCADE` into its cards and loaded files, which the conversation share lock (`db::share_lock_conversations_for`) orders instead. The only other advisory lock is sqlx's own, taken while migrations run. |

### Settings

| Table | What it holds | Notes |
|---|---|---|
| `ssh_keys` | `name`, `public_key`, `private_key` (plain text for now) | `name` unique, and the unique index `ssh_keys_only_one` on `(true)` allows one row. |
| `git_settings` | The commit identity: `author_name`, `author_email` | Single row: `id` is a `BOOLEAN` PK defaulting to and `CHECK`ed `TRUE`. |
| `smelt_instance` | This database's identity in the cluster (SME-115): `instance_id` (a UUID), `owns_unlabelled`, `created_at` | Single row, like `git_settings`; made by its migration and never changed. Every pod and claim smelt creates is labelled `smelt/instance=<instance_id>`, and smelt only deletes, reuses, mounts or watches objects with its own (see [architecture.md](architecture.md)). `owns_unlabelled` is fixed at migrate time: true when the database already had conversations or volumes (the dev and production databases), so only that database adopts the unlabelled objects made before the fix. Read with `db::smelt_instance(pool)`, never from a global: tests run many databases in one process. |
| `mcp_servers` | `name`, `url`, `extra_headers` JSONB, `auth_mode`, `oauth_credentials` JSONB, `oauth_client_id`, `oauth_client_secret` | `name` unique. `auth_mode` `CHECK`ed to `static_headers`/`oauth`. A name holding `/` or `*` can never be named by a tool-rule pattern (SME-241), so `api::mcp`'s create and update refuse one (`mcp::check_server_name`) — no `CHECK`, since rows named that way before must survive; they're reported instead (`McpServerSummary::name_unrulable`). |
| `mcp_tool_classes` | A user's class override for one of a server's tools (SME-240): `class` (`read-only`/`writing`/`destructive`, `CHECK`ed — the `auth_mode` precedent: the DB is the source of truth for valid values), `created_at`, `updated_at` | PK `(server_id, tool_name)` — keyed by the server's id, not its name, so renaming a server keeps its overrides; FK → `mcp_servers` cascade, so deleting a server takes its overrides with it. The absence of a row means "the server's answer"; `set_mcp_tool_class` upserts, `clear_mcp_tool_class` deletes (and says whether anything matched). |
| `inference_providers` | A model provider (SME-72): `name`, `kind`, `base_url`, `auth_kind`, `secret` (plain text for now), `prompt_caching`, `price_catalog_provider` | `name` unique. `kind` `CHECK`ed to `anthropic`/`ollama`/`llama_cpp`/`other`, `auth_kind` to `api_key`/`bearer`. `keep_reasoning` and `server_caps` (SME-111, `llama_cpp` only): whether a turn sends the chat template `preserve_thinking`, and the server's `/props` as `providers::LlamaServerInfo` (JSONB, written by its model listing on the provider's page and by the picker's refresh, null until first read, and cleared when the provider's address or kind changes, a read finishing after such a change being dropped (SME-162 review 2); each write bumps `updated_at`; the picker's quick listing applies it over `/v1/models`' details). A turn sends the template `reasoning_effort` and `preserve_thinking` only when `server_caps` says the template reads them. `price_catalog_provider` (SME-106): the models.dev provider id its calls are priced as, or null; read into `TurnModel` when a turn starts. `prompt_caching` (SME-106): a turn's request carries `cache_control` breakpoints (`CreateMessageRequest::to_body`), read once per turn into `TurnModel`; the form starts it on for `anthropic` and off for the other kinds, whose server may refuse the field. Compaction's request never carries them. `max_concurrent_requests` (SME-162): how many model requests smelt sends the provider at once, `CHECK`ed `>= 0`: null no limit, `0` (a `llama_cpp` provider only, `providers::checked`) the slot count its `server_caps` reports (no limit until one is read), else that many; a request over it waits for a slot (`providers::limiter`). |
| `provider_models` | What's known about a model: the user's `thinking`/`context_window`/`effort` (SME-106: sent as `output_config.effort` on an Anthropic provider's turns, as the chat template's `reasoning_effort` on a llama.cpp provider's when its template reads it (SME-111), null sends none), `max_output` (SME-111: "Max reply tokens", caps a turn's reply budget), `reasoning_budget` (SME-111: caps a llama.cpp turn's `thinking.budget_tokens`), what the provider last reported (`reported_context_window` and `reported_max_output` (Anthropic's listing's `max_tokens`), each kept when a report lacks one, `reported_thinking`, `reported_tools`, `reported_adaptive_thinking` (B9, SME-122: Anthropic's listing's `capabilities.thinking.types.adaptive.supported`; false sends a model such as Haiku 4.5 a `thinking.budget_tokens` instead of adaptive thinking)), and `added_by_hand` | PK `(provider_id, model)`. A row that's neither added by hand nor overridden is shown only while the provider still lists the model. FK → `inference_providers` cascade. |
| `inference_settings` | The default model: `default_provider_id`, `default_model` | Single row, like `git_settings`. The two are both set or both null (`CHECK`). FK → `inference_providers` `RESTRICT`: `delete_inference_provider` clears it and every conversation using the provider, then deletes, in one transaction, so a reference it missed fails the delete. |
| `language_servers` | A language server config: `name`, `image`, `install_command`, `command`, `args`/`env`/`file_types`/`root_markers`/`initialization_options`/`settings` (JSONB), `memory_limit`, `cpu_limit`, `enabled` | `name` unique. Its pods aren't recorded: Kubernetes is their record. |

### Deleting a conversation

Every FK in the schema is `ON DELETE CASCADE`, so deleting a `conversations` row removes:

```
conversations
├── messages
├── conversation_context_usage
├── model_call_usage
├── conversation_todos
├── pending_questions
├── agents             (by conversation_id, and by root_id)
├── agent_messages     (to it; one it sent keeps a null from_conversation_id)
├── conversations      (its children, by parent_id, and theirs)
├── sandbox_pods
│   ├── sandbox_pod_previews
│   └── sandbox_terminals
│       └── terminal_commands
│           └── terminal_events
├── terminal_commands (also directly, by conversation_id)
└── conversation_repos
    ├── loaded_instructions   (also directly, by conversation_id)
    └── instruction_requests  (also directly, by conversation_id)
```

This only covers rows: the pod and its Kubernetes resources are torn down separately by `api::chat::delete_conversation` (`sandbox::teardown_conversation`), for every conversation in the subtree (`agents::delete_subtree`, SME-236: it first marks the subtree `deleting_at` and its agents `stopped` under the group root's lock, then deletes with a notice to the deleted agent's parent in the same transaction). `repo_trust`, `ssh_keys`, `git_settings`, `mcp_servers`, `sandbox_volumes` and `language_servers` belong to no conversation and are untouched.

## Testing against the database

See [testing.md](testing.md#database-tests) — `#[sqlx::test]` gives every test its own isolated, migrated Postgres database, passed in as a `PgPool` argument.
