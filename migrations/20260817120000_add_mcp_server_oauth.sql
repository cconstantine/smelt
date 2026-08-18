-- Adds a second auth mode to mcp_servers alongside static extra_headers —
-- see docs/projects/plans/mcp-oauth.md.
--
-- oauth_credentials stores rmcp::transport::auth::StoredCredentials
-- serialized as-is (client_id, tokens, granted scopes, issuer) — one JSONB
-- blob, same precedent as extra_headers, rather than hand-modeled columns
-- for a shape rmcp already owns.

ALTER TABLE mcp_servers
    ADD COLUMN auth_mode TEXT NOT NULL DEFAULT 'static_headers'
        CHECK (auth_mode IN ('static_headers', 'oauth')),
    ADD COLUMN oauth_credentials JSONB;
