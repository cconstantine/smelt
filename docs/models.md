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

Always derive: `Clone, Debug, Serialize, Deserialize, PartialEq`. `sqlx::FromRow` is gated behind `#[cfg_attr(feature = "server", ...)]` since the derive itself references sqlx types that don't exist in the web build.

`Message.role` is a plain `String` (`"user"` / `"assistant"`, enforced by a `CHECK` constraint at the database level). `Message.content` is still a plain `TEXT` column at the database level — one string, same as before — but its *contents* changed meaning once tool-use landed: it now holds `serde_json::to_string`-serialized `Vec<anthropic::ContentBlock>`, not raw text (`[{"type":"text","text":"hello"}]` rather than `hello`). A one-time migration backfilled existing rows into that shape.

```rust
impl Message {
    pub fn blocks(&self) -> Result<Vec<ContentBlock>, serde_json::Error> {
        serde_json::from_str(&self.content)
    }
}
```

`Message::blocks()` parses the stored JSON back into `ContentBlock`s — callers (persistence in `db::create_message`, rendering in `frontend/pages/chat.rs`, history-building in `api::chat::run_turn`) always go through `blocks()`/a `&[ContentBlock]` parameter rather than touching `content` as a string directly. A parse error is a real possibility (malformed content shouldn't happen but isn't structurally prevented) — every caller surfaces it rather than silently rendering blank, per `development-process.md`'s "surface fallback outcomes" rule.

Don't confuse `models::Message` (a database row) with `anthropic::AnthropicMessage` (an Anthropic API wire message, `{role, content: Vec<ContentBlock>}`) — `run_turn` converts between them when building a request and when persisting a turn.
