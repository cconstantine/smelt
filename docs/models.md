# Models

`src/models.rs` holds the shared row/wire types — compiled into both the `server` and `web` targets, since they cross the client/server boundary as server-function arguments and return values.

```rust
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct Conversation {
    pub id: i64,
    pub title: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct Message {
    pub id: i64,
    pub conversation_id: i64,
    pub role: String,
    pub content: String,
    pub created_at: NaiveDateTime,
}
```

Always derive: `Clone, Debug, Serialize, Deserialize, PartialEq`. Derive `sqlx::FromRow` only where the struct maps a table row one-to-one, gated behind `#[cfg_attr(feature = "server", ...)]` since the derive itself references sqlx types that don't exist in the web build.

`Message.role` is a plain `String` (`"user"` / `"assistant"`, enforced by a `CHECK` constraint at the database level). `Message.content` is still a plain `TEXT` column at the database level — one string, same as before — but its *contents* changed meaning once tool-use landed: it now holds `serde_json::to_string`-serialized `Vec<anthropic::ContentBlock>`, not raw text (`[{"type":"text","text":"hello"}]` rather than `hello`). A one-time migration backfilled existing rows into that shape.

```rust
impl Message {
    pub fn blocks(&self) -> Result<Vec<ContentBlock>, serde_json::Error> {
        serde_json::from_str(&self.content)
    }
}
```

`Message::blocks()` parses the stored JSON back into `ContentBlock`s — callers (persistence in `db::create_message`, rendering in `frontend/pages/chat/transcript.rs`, history-building in `turn::history_for_request`) always go through `blocks()`/a `&[ContentBlock]` parameter rather than touching `content` as a string directly. A parse error is a real possibility (malformed content shouldn't happen but isn't structurally prevented) — every caller surfaces it rather than silently rendering blank, per `development-process.md`'s "surface fallback outcomes" rule.

Two `ContentBlock` variants exist only for smelt's own bookkeeping, never sent or accepted by Anthropic itself — `CompactionSummary` and `CompactionPlaceholder` (auto-compaction's own output — see SME-18). `history_for_request` translates both into plain `Text` blocks before they're ever replayed to the real API; storage and rendering see the real variant.

Don't confuse `models::Message` (a database row) with `anthropic::AnthropicMessage` (an Anthropic API wire message, `{role, content: Vec<ContentBlock>}`) — `run_turn` converts between them when building a request and when persisting a turn.

## Language servers

A configured language server (SME-35), what the language servers settings page edits:

```rust
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LanguageServer {
    pub id: i64,
    #[serde(flatten)]
    pub config: LanguageServerConfig,
    pub updated_at: NaiveDateTime,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LanguageServerConfig {
    pub name: String,
    pub image: String,
    pub install_command: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub file_types: BTreeMap<String, String>, // extension (no dot) → LSP language id
    pub root_markers: Vec<String>,
    pub initialization_options: Option<serde_json::Value>,
    pub settings: Option<serde_json::Value>,
    pub memory_limit: String,
    pub cpu_limit: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LanguageServerSuggestion {
    pub config: LanguageServerConfig,
    pub notes: Vec<String>,
}
```

`LanguageServerConfig::validate()` returns `Err` with the message the page shows (a bad name, image, command, file type or limit), or `Ok(())`; `lsp::config::save` calls it before writing. `LanguageServerSuggestion` is a catalog lookup's suggested config plus notes on what the user should check before saving it.

`LanguageServer` has no `FromRow`: its list and map columns are JSONB and its config is `#[serde(flatten)]`ed, so it isn't a one-to-one row. `db.rs` reads it through a private `LanguageServerRow` and converts with `From` (see [database.md](database.md#query-pattern)).

## Other types

Model providers' wire types (`ProviderSummary`, `ProviderInput`, `ProviderKind`, `AuthKind`, `LlamaServerInfo`, `ModelInfo`, `ProviderModels`, `ModelChoice`, `ConversationModel`) are in `src/providers.rs` rather than here, next to the logic that fills them (SME-72). `ConversationModel` is internally tagged (`state`) with struct variants, round-tripped in its tests.

`models.rs` isn't the only home for these:

- Server-only row types live in `db.rs` (`SandboxPod`, `SandboxTerminal`, `TerminalCommand`, `SandboxVolume`, `McpServerConfig`, `ConversationRepo`, `LoadedInstruction`, `SshKey`, ...), deriving `FromRow` ungated since `db` is only compiled with the `server` feature.
- Shared wire types live next to the code they belong to: `anthropic::types` (`ContentBlock`, `TokenUsage`, `AnthropicMessage`, ...), `events.rs` (`ConversationEvent`, `AppEvent`, `SandboxPreview`) and `git.rs` (`RepoSummary`, `SshKeySummary`, `GitIdentity`, ...).
