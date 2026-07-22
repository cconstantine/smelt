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

`Message.role` is a plain `String` (`"user"` / `"assistant"`, enforced by a `CHECK` constraint at the database level — see [migrations.md](migrations.md)), and `Message.content` is plain text, not the Anthropic `ContentBlock` wire shape (see [architecture.md](architecture.md) for why — v1 has exactly one content shape, so a richer stored structure would be speculative). Don't confuse `models::Message` (a database row) with `anthropic::AnthropicMessage` (an Anthropic API wire message) — `send_message` converts between them when building a request.
