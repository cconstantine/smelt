use super::*;

// --- MCP servers (externally-hosted, configured via the /mcp-servers UI) ---
// Plain CRUD, no soft delete — this is configuration a person edits, not a
// live external resource like a sandbox pod. See
// SME-15.

/// Holds header values and OAuth secrets, so it has no `Serialize` and its
/// `Debug` hides them (SME-60): the UI gets `api::mcp::McpServerSummary`.
#[derive(Clone, PartialEq, sqlx::FromRow)]
pub struct McpServerConfig {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub extra_headers: sqlx::types::Json<HashMap<String, String>>,
    /// `"static_headers"` or `"oauth"` — see
    /// SME-16. A plain `String`, not a Rust enum:
    /// the DB-level `CHECK` constraint is the source of truth for valid
    /// values, matching `terminal_commands.status`'s existing precedent
    /// elsewhere in this file.
    pub auth_mode: String,
    /// `rmcp::transport::auth::StoredCredentials`, serialized as-is —
    /// `NULL` means "oauth mode, never connected (or disconnected)".
    /// Untyped `serde_json::Value` here (not the `StoredCredentials` type
    /// itself) since this module doesn't otherwise depend on `rmcp`;
    /// `PgCredentialStore` (`src/mcp_oauth.rs`) does the typed
    /// (de)serialization at its load/save boundary.
    pub oauth_credentials: Option<sqlx::types::Json<serde_json::Value>>,
    /// A pre-registered OAuth client, for a provider that publishes no
    /// discovery metadata and rejects Dynamic Client Registration — GitHub,
    /// confirmed live (see SME-16). Set only at
    /// creation time (`McpServerNew`), same "delete and recreate to
    /// change" precedent as `auth_mode` itself. `oauth_client_id` isn't
    /// secret — it's visible in the authorization URL sent to the browser
    /// regardless — so it's shown back plain, unlike `oauth_client_secret`
    /// (write-only, `extra_headers`' precedent).
    pub oauth_client_id: Option<String>,
    pub oauth_client_secret: Option<String>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

impl std::fmt::Debug for McpServerConfig {
    /// Header names, the client id and whether credentials are stored, but
    /// no header value, credential or client secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut header_names: Vec<&String> = self.extra_headers.0.keys().collect();
        header_names.sort();
        f.debug_struct("McpServerConfig")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("url", &self.url)
            .field("extra_headers", &header_names)
            .field("auth_mode", &self.auth_mode)
            .field("oauth_credentials", &self.oauth_credentials.as_ref().map(|_| "<stored>"))
            .field("oauth_client_id", &self.oauth_client_id)
            .field("oauth_client_secret", &self.oauth_client_secret.as_ref().map(|_| "<set>"))
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

#[cfg(test)]
mod mcp_server_config_debug_tests {
    use super::*;

    #[test]
    fn test_an_mcp_server_configs_debug_output_hides_its_secrets() {
        let config = McpServerConfig {
            id: 7,
            name: "github".to_string(),
            url: "https://api.githubcopilot.com/mcp/".to_string(),
            extra_headers: sqlx::types::Json(HashMap::from([(
                "Authorization".to_string(),
                "Bearer header-secret-123".to_string(),
            )])),
            auth_mode: "oauth".to_string(),
            oauth_credentials: Some(sqlx::types::Json(serde_json::json!({"access_token": "token-secret-456"}))),
            oauth_client_id: Some("Iv1.public-client-id".to_string()),
            oauth_client_secret: Some("client-secret-789".to_string()),
            created_at: NaiveDateTime::default(),
            updated_at: NaiveDateTime::default(),
        };
        let shown = format!("{config:?}");
        for secret in ["header-secret-123", "token-secret-456", "client-secret-789"] {
            assert!(!shown.contains(secret), "Debug output shows {secret}: {shown}");
        }
        for kept in ["github", "Authorization", "Iv1.public-client-id", "oauth"] {
            assert!(shown.contains(kept), "Debug output should still show {kept}: {shown}");
        }
    }
}

pub async fn create_mcp_server_config(
    pool: &PgPool,
    name: &str,
    url: &str,
    extra_headers: &HashMap<String, String>,
    auth_mode: &str,
    oauth_client_id: Option<&str>,
    oauth_client_secret: Option<&str>,
) -> Result<McpServerConfig, sqlx::Error> {
    sqlx::query_as::<_, McpServerConfig>(
        "INSERT INTO mcp_servers (name, url, extra_headers, auth_mode, oauth_client_id, oauth_client_secret)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
    )
    .bind(name)
    .bind(url)
    .bind(sqlx::types::Json(extra_headers))
    .bind(auth_mode)
    .bind(oauth_client_id)
    .bind(oauth_client_secret)
    .fetch_one(pool)
    .await
}

pub async fn list_mcp_server_configs(pool: &PgPool) -> Result<Vec<McpServerConfig>, sqlx::Error> {
    sqlx::query_as::<_, McpServerConfig>("SELECT * FROM mcp_servers ORDER BY name ASC")
        .fetch_all(pool)
        .await
}

pub async fn get_mcp_server_config(
    pool: &PgPool,
    id: i64,
) -> Result<Option<McpServerConfig>, sqlx::Error> {
    sqlx::query_as::<_, McpServerConfig>("SELECT * FROM mcp_servers WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Resolves a `mcp__<server_name>__<tool_name>` tool call's server half —
/// `name` is unique, so this is how `anthropic::tools::execute`'s MCP
/// dispatch arm turns the model-facing name back into a row.
pub async fn get_mcp_server_config_by_name(
    pool: &PgPool,
    name: &str,
) -> Result<Option<McpServerConfig>, sqlx::Error> {
    sqlx::query_as::<_, McpServerConfig>("SELECT * FROM mcp_servers WHERE name = $1")
        .bind(name)
        .fetch_optional(pool)
        .await
}

/// Updates a server's name, URL, and headers in one shot — the single save
/// action the `/mcp-servers/{id}` edit page's one form drives. Name/URL are
/// set outright; headers are *merged*: `upsert` adds/overwrites just those
/// names and `remove` drops just those names, so every other existing
/// header (including ones the browser never received a value for — see
/// `McpServerSummary`) is left exactly as it was. If a name appears in both
/// `upsert` and `remove`, `upsert` wins — the header ends up set, not
/// deleted (arbitrary but deterministic; the UI never actually produces
/// this case, since a header row is either edited or removed, never both).
/// A URL change clears any stored `oauth_credentials` — see
/// SME-16's "Data model": a grant is bound to the
/// audience it was issued for, so carrying it across a URL change would
/// silently point a stale token at a different resource.
pub async fn update_mcp_server_config(
    pool: &PgPool,
    id: i64,
    name: &str,
    url: &str,
    upsert: &HashMap<String, String>,
    remove: &[String],
    auth_mode: &str,
) -> Result<Option<McpServerConfig>, sqlx::Error> {
    let Some(existing) = get_mcp_server_config(pool, id).await? else {
        return Ok(None);
    };

    let mut merged = existing.extra_headers.0;
    for name in remove {
        merged.remove(name);
    }
    merged.extend(upsert.iter().map(|(k, v)| (k.clone(), v.clone())));

    // Whether the URL changes is decided against the row being written
    // over, not `existing`: another edit and a sign-in's save can commit
    // between the read above and this write, and a grant for their URL
    // must not survive onto this one (SME-166 review 1). Postgres
    // re-reads a row changed under it before applying the `SET`.
    sqlx::query_as::<_, McpServerConfig>(
        "UPDATE mcp_servers SET name = $2, url = $3, extra_headers = $4, auth_mode = $5,
             oauth_credentials = CASE WHEN url IS DISTINCT FROM $3 THEN NULL
                                      ELSE oauth_credentials END,
             updated_at = now()
         WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .bind(name)
    .bind(url)
    .bind(sqlx::types::Json(merged))
    .bind(auth_mode)
    .fetch_optional(pool)
    .await
}

/// Adds an MCP server named `name` unless one with that name already
/// exists, for the servers smelt ships with (`mcp::default_mcp_servers`).
/// Matched by name only, so a user's edits to the entry (a key header, a
/// new URL, OAuth) are kept. Returns whether it inserted.
pub async fn ensure_mcp_server(pool: &PgPool, name: &str, url: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO mcp_servers (name, url) VALUES ($1, $2)
         ON CONFLICT (name) DO NOTHING",
    )
    .bind(name)
    .bind(url)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn delete_mcp_server_config(pool: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM mcp_servers WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Sets or clears a server's stored OAuth credentials — the persistence
/// half of `mcp_oauth::PgCredentialStore`'s `save`/`clear`. `None` clears
/// (the same as a fresh `oauth` server that has never connected).
pub async fn set_mcp_server_oauth_credentials(
    pool: &PgPool,
    id: i64,
    credentials: Option<serde_json::Value>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE mcp_servers SET oauth_credentials = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(credentials.map(sqlx::types::Json))
        .execute(pool)
        .await?;
    Ok(())
}

/// Saves a new sign-in's grant only while the row's URL is still `url`,
/// the address the sign-in started for (SME-166): a sign-in that finishes
/// after a URL edit or a delete mustn't put the old address's token on the
/// new one. One statement, so Postgres orders it against the edit's own
/// `UPDATE` of the row. `false` when nothing matched.
pub async fn save_mcp_server_sign_in(
    pool: &PgPool,
    id: i64,
    url: &str,
    credentials: serde_json::Value,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE mcp_servers SET oauth_credentials = $3, updated_at = now() \
         WHERE id = $1 AND url = $2",
    )
    .bind(id)
    .bind(url)
    .bind(sqlx::types::Json(credentials))
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() == 1)
}

/// Saves a refreshed grant only while the row still holds a grant for the
/// same client: a refresh that finishes after a Disconnect, a URL change
/// or a new Connect mustn't write back over what they wrote (SME-113).
/// And only while the row's URL is still `url`, the address the
/// refreshing connection was built for (SME-166 review 2). `false` when
/// nothing matched.
pub async fn save_refreshed_mcp_server_oauth_credentials(
    pool: &PgPool,
    id: i64,
    url: &str,
    client_id: &str,
    credentials: serde_json::Value,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE mcp_servers SET oauth_credentials = $4, updated_at = now() \
         WHERE id = $1 AND url = $2 AND oauth_credentials->>'client_id' = $3",
    )
    .bind(id)
    .bind(url)
    .bind(client_id)
    .bind(sqlx::types::Json(credentials))
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() == 1)
}
