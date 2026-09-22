-- Last-known-only, per conversation (not a history-over-time table — the
-- todo panel and the model's own todoread only ever need the current list,
-- not a log of every past state). Mirrors conversation_context_usage's
-- shape exactly — see docs/projects/plans/todo-list-tool.md.
CREATE TABLE conversation_todos (
    conversation_id BIGINT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
    items JSONB NOT NULL DEFAULT '[]',
    updated_at TIMESTAMP NOT NULL DEFAULT now()
);
