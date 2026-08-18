-- Some OAuth servers (GitHub confirmed live — see
-- docs/projects/plans/mcp-oauth.md's "Live verification" retrospective
-- note) publish no RFC 9728/8414 discovery metadata at all and reject
-- Dynamic Client Registration outright, requiring a client_id (and, since
-- they're confidential clients, a client_secret) pre-registered by hand
-- with the provider instead. Both are set only at server-creation time
-- (McpServerNew) — same "delete and recreate to change" precedent already
-- applied to auth_mode itself.
--
-- oauth_client_id is not secret (it's visible in the authorization URL
-- sent to the browser regardless) so it's stored and shown back plain,
-- same as name/url. oauth_client_secret follows extra_headers' write-only
-- precedent: never returned to the browser.

ALTER TABLE mcp_servers
    ADD COLUMN oauth_client_id TEXT,
    ADD COLUMN oauth_client_secret TEXT;
