//! An OAuth server's shared `AuthClient` and the status page's sign-in check.

use super::*;

/// An OAuth server's HTTP client: rmcp's `AuthClient`, which gets a token
/// from the server's `AuthorizationManager` before every request.
type OAuthClient = rmcp::transport::auth::AuthClient<reqwest_rmcp::Client>;

pub(super) struct SharedOAuthClient {
    pub(super) client: OAuthClient,
    /// What the manager was built for: the server's URL and the stored
    /// grant's client id. A manager built for anything else isn't reused.
    url: String,
    grant_client_id: Option<String>,
}

/// One `AuthClient` per OAuth server, cloned into each of its connections
/// and used by the status check (SME-113). Clones share one manager behind
/// one mutex, so refreshes are one at a time: with GitHub rotating refresh
/// tokens on every refresh, two managers refreshing at once would retire
/// each other's tokens. `evict` drops the entry (an edit, a delete, a
/// Disconnect, a new grant), and an entry built for another URL or another
/// grant's client is rebuilt rather than reused, which also covers a build
/// that finishes after an `evict`.
pub(super) static OAUTH_CLIENTS: LazyLock<AsyncMutex<HashMap<i64, SharedOAuthClient>>> =
    LazyLock::new(|| AsyncMutex::new(HashMap::new()));

/// One lock per server, held while building its `AuthClient`, so two
/// callers don't build two managers.
static BUILDING_OAUTH: LazyLock<std::sync::Mutex<HashMap<i64, Arc<AsyncMutex<()>>>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

fn grant_client_id(config: &McpServerConfig) -> Option<String> {
    config
        .oauth_credentials
        .as_ref()
        .and_then(|json| json.0.get("client_id")?.as_str().map(str::to_string))
}

/// The server's shared `AuthClient`, built on first use.
pub(super) async fn oauth_client(
    pool: &sqlx::PgPool,
    config: &McpServerConfig,
) -> Result<OAuthClient, String> {
    let grant_client_id = grant_client_id(config);
    let lock = server_lock(&BUILDING_OAUTH, config.id);
    let _building = lock.lock().await;
    if let Some(shared) = OAUTH_CLIENTS.lock().await.get(&config.id)
        && shared.url == config.url
        && shared.grant_client_id == grant_client_id
    {
        return Ok(shared.client.clone());
    }
    let manager = crate::mcp_oauth::connection_manager(pool, config).await?;
    let client = OAuthClient::new(transport_http_client()?, manager);
    OAUTH_CLIENTS.lock().await.insert(
        config.id,
        SharedOAuthClient {
            client: client.clone(),
            url: config.url.clone(),
            grant_client_id,
        },
    );
    Ok(client)
}

pub(super) fn sign_in_expired(config: &McpServerConfig) -> String {
    format!(
        "the sign-in to MCP server {:?} has expired or was revoked; use Reconnect",
        config.name
    )
}

/// For an OAuth server, gets a token from its shared manager: a store read,
/// or a refresh when it's expiring. A grant that can't be refreshed drops
/// the server's cached connection, which would otherwise still report
/// Connected (SME-113).
/// The server's stored grant as it is now, to tell whether another
/// refresh replaced it. `None` too when it can't be read.
async fn stored_grant(pool: &sqlx::PgPool, server_id: i64) -> Option<serde_json::Value> {
    crate::db::get_mcp_server_config(pool, server_id)
        .await
        .ok()
        .flatten()
        .and_then(|config| config.oauth_credentials)
        .map(|json| json.0)
}

pub(super) async fn check_sign_in(pool: &sqlx::PgPool, config: &McpServerConfig) -> Result<(), CheckError> {
    let token = tokio::time::timeout(CONNECT_TIMEOUT, async {
        let client = oauth_client(pool, config)
            .await
            .map_err(CheckError::Unreachable)?;
        // In a task of its own: a refresh the provider has received
        // rotates its refresh token whether or not anyone waits for the
        // reply, so the reply must still be saved when this check times
        // out or its request goes away (review 2).
        let fetch = |client: OAuthClient| async move {
            tokio::spawn(async move { client.get_access_token().await })
                .await
                .map_err(|e| {
                    CheckError::Unreachable(format!(
                        "getting an OAuth token for MCP server {:?} failed: {e}",
                        config.name
                    ))
                })
        };
        let grant_before = stored_grant(pool, config.id).await;
        let mut fetched = fetch(client.clone()).await?;
        use rmcp::transport::auth::AuthError::{AuthorizationRequired, InternalError};
        if matches!(fetched, Err(AuthorizationRequired | InternalError(_)))
            && stored_grant(pool, config.id).await != grant_before
        {
            // The provider refused this refresh, or its save lost the
            // compare-and-set (SME-177), but the grant changed meanwhile:
            // another manager refreshed first with the same refresh token
            // (review 2), or a Reconnect saved a new one. That's what counts.
            fetched = fetch(client).await?;
        }
        fetched.map_err(|e| match e {
            rmcp::transport::auth::AuthError::AuthorizationRequired => {
                CheckError::SignInExpired(sign_in_expired(config))
            }
            other => CheckError::Unreachable(format!(
                "couldn't get an OAuth token for MCP server {:?}: {other}",
                config.name
            )),
        })
    })
    .await
    .map_err(|_| {
        CheckError::Unreachable(format!(
            "MCP server {:?}'s OAuth provider didn't answer within {}s",
            config.name,
            CONNECT_TIMEOUT.as_secs()
        ))
    })?;
    if let Err(CheckError::SignInExpired(_)) = &token {
        evict(config.id).await;
    }
    token.map(|_| ())
}
