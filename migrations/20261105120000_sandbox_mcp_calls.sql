-- SME-359: one row per MCP call a script in a sandbox made, for the
-- sandbox panel's and the model's per-command summary and the per-command
-- budget. Never the call's arguments or result (SME-137): an argument can
-- carry a secret the script read.
CREATE TABLE sandbox_mcp_calls (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    conversation_id BIGINT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    pod_id          BIGINT NOT NULL REFERENCES sandbox_pods(id) ON DELETE CASCADE,
    -- The terminal command the calling process belonged to, when the agent
    -- could tell; NULL for one outside any command.
    command_id      TEXT REFERENCES terminal_commands(command_id) ON DELETE SET NULL,
    server          TEXT NOT NULL,
    tool            TEXT NOT NULL,
    -- NULL when the call was refused before its class was known.
    class           TEXT,
    outcome         TEXT NOT NULL
        CHECK (outcome IN ('running', 'ok', 'tool_error', 'refused', 'limited', 'failed')),
    started_at      TIMESTAMP NOT NULL DEFAULT now(),
    finished_at     TIMESTAMP
);

CREATE INDEX idx_sandbox_mcp_calls_command ON sandbox_mcp_calls(command_id);
CREATE INDEX idx_sandbox_mcp_calls_conversation ON sandbox_mcp_calls(conversation_id, started_at);
