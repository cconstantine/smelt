# Database

Postgres via sqlx. `db.rs`'s CRUD functions take `pool: &PgPool` as an explicit parameter rather than reaching into a global — that's what lets `#[sqlx::test]` (see [testing.md](testing.md#database-tests)) hand each test its own isolated database.

A process-wide `OnceLock<PgPool>` still exists for *production* wiring, so the pool is opened once, at startup:

```rust
// src/db.rs
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
| `conversations` | `id`, `title`, `created_at`, `updated_at`, `provider_id`, `model`, `last_turn_model_key`, `model_changed_at_message_id` | `provider_id`/`model`: the conversation's model, both null until its first turn takes the default (a `CHECK` keeps them together; FK → `inference_providers` `RESTRICT`, cleared by `delete_inference_provider`). `last_turn_model_key`: the backend (provider, base URL, model) its last turn ran on; `model_changed_at_message_id`: its last message when a turn started on a different one, so thinking signed for the old one isn't replayed. Both set at turn start, under the turn lock (SME-72). `title`'s column default is `'New Conversation'`, but `db::create_conversation` always binds `DEFAULT_TITLE` (`"New conversation"`). Auto-titling from the first user message matches either spelling, case-insensitively. |
| `messages` | `conversation_id`, `role`, `content` (JSON-serialized content blocks as `TEXT`, see [models.md](models.md)) | `role` `CHECK`ed to `user`/`assistant`. FK → `conversations` cascade. |
| `conversation_context_usage` | The last turn's four token counts | PK is `conversation_id` (one row per conversation). FK → `conversations` cascade. |
| `model_call_usage` | One row per completed model call (SME-106): `provider_id`, `model`, `kind` (`turn`/`compaction`), the four token counts, `cost_usd`, `created_at` | Append-only. Written by `db::record_model_call`, which for a turn's call also upserts `conversation_context_usage` in the same transaction; a compaction's isn't the last-known usage. A stopped or failed call has no final usage and no row. `cost_usd` is null without a price and never recomputed. FK → `conversations` cascade, → `inference_providers` `SET NULL` (the model and tokens stay). `db::get_conversation_spend` sums a conversation's rows for the context detail view, and `db::model_spend_since` each provider's models' rows for the providers page's last 30 days (a deleted provider's under none). |
| `price_catalog` | The last models.dev price catalog fetched (SME-106): `fetched_at`, `providers` (JSONB, `pricing::Providers`) | Single row, like `git_settings`. Written only by `pricing::CatalogStore::refresh` (startup, then hourly); read once at startup so a restart without network has prices. |
| `conversation_todos` | `items` JSONB, the todo list | PK is `conversation_id`. FK → `conversations` cascade. |
| `pending_questions` | `tool_use_id`, `questions` JSONB (the model's `ask_user` input), `answer` JSONB (null while waiting) | PK is `conversation_id`: at most one per conversation. FK → `conversations` cascade. The card's answer is written once (`WHERE answer IS NULL`); the next turn deletes the row in the same transaction that saves the call's result (SME-34). |

### Sandbox

| Table | What it holds | Notes |
|---|---|---|
| `sandbox_pods` | A conversation's pod: `conversation_id`, `terminated_at` | Soft delete. Partial unique index `sandbox_pods_one_live_per_conversation`: at most one live pod per conversation. FK → `conversations` cascade. |
| `sandbox_terminals` | A terminal in a pod: `pod_id`, `terminated_at` | Soft delete. FK → `sandbox_pods` cascade. |
| `terminal_commands` | A command run in a terminal: `conversation_id`, `terminal_id`, `command_id` (text), `command`, `status`, `exit_code`, `notified_at`, `finished_at` | `command_id` unique. `status` `CHECK`ed to `running`/`finished`/`lost`. FKs → `conversations` and `sandbox_terminals`, both cascade. |
| `terminal_events` | A command's output: `command_id`, `stream`, `seq`, `data` | `stream` `CHECK`ed to `stdout`/`stderr`. FK → `terminal_commands(command_id)` cascade. |
| `sandbox_pod_previews` | Ports shared as previews: `pod_id`, `host` (`''` is the pod's localhost), `port` | PK `(pod_id, host, port)`; `port` `CHECK`ed to 1–65535. FK → `sandbox_pods` cascade. |
| `sandbox_volumes` | Volumes mounted into every pod: `name`, `mount_path` | `name` unique. No FKs. |

### Git

| Table | What it holds | Notes |
|---|---|---|
| `conversation_repos` | A repo a conversation works on: `url`, `remote_key`, `branch`, `dir`, `status`, `error`, `checked_out_branch`, `commit_sha`, `agents_files` (`TEXT[]`), `attempt` | `UNIQUE (conversation_id, dir)`. `status` `CHECK`ed to `cloning`/`ready`/`failed`. Only a `failed` row is retried, which increments `attempt`; ready/failed are written only for the current attempt while `cloning` (SME-86). FK → `conversations` cascade. |
| `loaded_instructions` | An AGENTS.md the model loaded: `path`, `content`, `file_bytes`, `hash`, `commit_sha`, `loaded_at` | `UNIQUE (repo_id, path)`. FKs → `conversations` and `conversation_repos`, both cascade. |
| `instruction_requests` | An AGENTS.md load waiting on the user's trust decision; same columns as `loaded_instructions` | `UNIQUE (repo_id, path)`. FKs → `conversations` and `conversation_repos`, both cascade. |
| `repo_trust` | Whether a remote's AGENTS.md is trusted: `remote_key`, `trusted`, `decided_at` | PK is `remote_key`. No row: not asked yet. No FKs, so it outlives conversations. |

### Settings

| Table | What it holds | Notes |
|---|---|---|
| `ssh_keys` | `name`, `public_key`, `private_key` (plain text for now) | `name` unique, and the unique index `ssh_keys_only_one` on `(true)` allows one row. |
| `git_settings` | The commit identity: `author_name`, `author_email` | Single row: `id` is a `BOOLEAN` PK defaulting to and `CHECK`ed `TRUE`. |
| `mcp_servers` | `name`, `url`, `extra_headers` JSONB, `auth_mode`, `oauth_credentials` JSONB, `oauth_client_id`, `oauth_client_secret` | `name` unique. `auth_mode` `CHECK`ed to `static_headers`/`oauth`. |
| `inference_providers` | A model provider (SME-72): `name`, `kind`, `base_url`, `auth_kind`, `secret` (plain text for now), `prompt_caching`, `price_catalog_provider` | `name` unique. `kind` `CHECK`ed to `anthropic`/`ollama`/`llama_cpp`/`other`, `auth_kind` to `api_key`/`bearer`. `keep_reasoning` and `server_caps` (SME-111, `llama_cpp` only): whether a turn sends the chat template `preserve_thinking`, and the server's `/props` as `providers::LlamaServerInfo` (JSONB, written by its model listing on the provider's page and by the picker's refresh, null until first read). A turn sends the template `reasoning_effort` and `preserve_thinking` only when `server_caps` says the template reads them. `price_catalog_provider` (SME-106): the models.dev provider id its calls are priced as, or null; read into `TurnModel` when a turn starts. `prompt_caching` (SME-106): a turn's request carries `cache_control` breakpoints (`CreateMessageRequest::to_body`), read once per turn into `TurnModel`; the form starts it on for `anthropic` and off for the other kinds, whose server may refuse the field. Compaction's request never carries them. |
| `provider_models` | What's known about a model: the user's `thinking`/`context_window`/`effort` (SME-106: sent as `output_config.effort` on an Anthropic provider's turns, as the chat template's `reasoning_effort` on a llama.cpp provider's when its template reads it (SME-111), null sends none), `max_output` (SME-111: "Max reply tokens", caps a turn's reply budget), `reasoning_budget` (SME-111: caps a llama.cpp turn's `thinking.budget_tokens`), what the provider last reported (`reported_context_window` and `reported_max_output` (Anthropic's listing's `max_tokens`), each kept when a report lacks one, `reported_thinking`, `reported_tools`), and `added_by_hand` | PK `(provider_id, model)`. A row that's neither added by hand nor overridden is shown only while the provider still lists the model. FK → `inference_providers` cascade. |
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

This only covers rows: the pod and its Kubernetes resources are torn down separately by `api::chat::delete_conversation` (`sandbox::teardown_conversation`). `repo_trust`, `ssh_keys`, `git_settings`, `mcp_servers`, `sandbox_volumes` and `language_servers` belong to no conversation and are untouched.

## Testing against the database

See [testing.md](testing.md#database-tests) — `#[sqlx::test]` gives every test its own isolated, migrated Postgres database, passed in as a `PgPool` argument.
