-- Externally-hosted MCP server configuration, managed via the /mcp-servers
-- UI. See docs/projects/plans/mcp-servers.md.
--
-- Plain CRUD, no soft delete: unlike sandbox_pods/sandbox_terminals this is
-- configuration a person edits, not a live external resource with its own
-- lifecycle to preserve history for.

CREATE TABLE mcp_servers (
    id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name           TEXT NOT NULL UNIQUE,
    url            TEXT NOT NULL,
    -- {"Authorization": "Bearer ..."} etc. — attached verbatim to every
    -- request to this server. See the plan's "Data model."
    extra_headers  JSONB NOT NULL DEFAULT '{}',
    created_at     TIMESTAMP NOT NULL DEFAULT now(),
    updated_at     TIMESTAMP NOT NULL DEFAULT now()
);
