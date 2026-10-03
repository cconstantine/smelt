-- The question a conversation is waiting on (SME-34): at most one per
-- conversation, from the model's ask_user call. `answer` is null while it
-- waits; the card writes it once (one tab wins), and the next turn to hold
-- the conversation's turn lock deletes the row and sends the answer as the
-- call's tool_result.
CREATE TABLE pending_questions (
    conversation_id BIGINT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
    tool_use_id TEXT NOT NULL,
    questions JSONB NOT NULL,
    answer JSONB,
    created_at TIMESTAMP NOT NULL DEFAULT now()
);
