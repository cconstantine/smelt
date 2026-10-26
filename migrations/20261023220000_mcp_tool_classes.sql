-- Per-tool tool-class overrides for MCP servers (SME-240).
-- The user's answer to a server that doesn't annotate its tools (or
-- annotates them wrongly): the class on an `mcp_tool_classes` row wins
-- over the server's annotations at every place a class is read.

-- Keyed by the server's id, not its name (docs/design/personas/tool-rules.md#mcp-tools):
-- renaming a server keeps its overrides; deleting the server cascades.
CREATE TABLE mcp_tool_classes (
    server_id  BIGINT NOT NULL REFERENCES mcp_servers(id) ON DELETE CASCADE,
    tool_name  TEXT   NOT NULL,
    -- TEXT with a CHECK, the auth_mode precedent: the DB is the source
    -- of truth for valid values. The three ToolClass names.
    class      TEXT   NOT NULL CHECK (class IN ('read-only','writing','destructive')),
    created_at TIMESTAMP NOT NULL DEFAULT now(),
    updated_at TIMESTAMP NOT NULL DEFAULT now(),
    PRIMARY KEY (server_id, tool_name)
);
