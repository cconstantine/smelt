-- A Claude Code conversation's CLI session (SME-430): which session the
-- next turn resumes, and what of smelt's history it has (the request's
-- first synced_count messages, by hash), so a turn sends only what's new
-- and rehydrates from smelt's history only when they diverge.
CREATE TABLE claude_cli_sessions (
    conversation_id BIGINT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
    provider_id BIGINT NOT NULL REFERENCES inference_providers(id) ON DELETE CASCADE,
    session_id TEXT NOT NULL,
    synced_count INTEGER NOT NULL CHECK (synced_count >= 0),
    synced_hash TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
