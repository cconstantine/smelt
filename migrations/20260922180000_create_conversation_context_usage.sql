-- Last-known-only, per conversation (not a history-over-time table — the
-- context-usage indicator only needs "how full is it right now", not a
-- token-usage graph). Separate from `conversations` itself so `Conversation`
-- (returned wholesale to the frontend for the sidebar list) doesn't grow
-- fields most of its callers never need — see
-- docs/projects/plans/auto-compaction.md.
CREATE TABLE conversation_context_usage (
    conversation_id BIGINT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
    input_tokens BIGINT NOT NULL,
    output_tokens BIGINT NOT NULL,
    cache_creation_input_tokens BIGINT NOT NULL,
    cache_read_input_tokens BIGINT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT now()
);
