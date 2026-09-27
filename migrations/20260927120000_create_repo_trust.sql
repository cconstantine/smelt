-- Whether the user trusts a remote's AGENTS.md as instructions for the
-- model (SME-32), asked once per remote (git::remote_key) and remembered
-- either way. No row: not asked yet.
CREATE TABLE repo_trust (
    remote_key TEXT PRIMARY KEY,
    trusted    BOOLEAN NOT NULL,
    decided_at TIMESTAMP NOT NULL DEFAULT now()
);

-- The AGENTS.md as last read from the checkout, which is what a trust
-- decision or a Reload loads: it can differ from what's loaded
-- (instructions_*) when the file changed, or when the remote isn't
-- trusted yet. Same shape as the instructions_* columns.
ALTER TABLE conversation_repos
    ADD COLUMN found_instructions TEXT,
    ADD COLUMN found_bytes        BIGINT,
    ADD COLUMN found_hash         TEXT,
    ADD COLUMN found_commit       TEXT,
    ADD COLUMN found_nested       TEXT[] NOT NULL DEFAULT '{}';
