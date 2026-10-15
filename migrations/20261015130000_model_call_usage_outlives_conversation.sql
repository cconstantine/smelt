-- A deleted conversation's model calls stay in model_call_usage, the
-- history the cost display sums (B50, SME-122): they were billed whether
-- or not the conversation is kept. ON DELETE CASCADE erased them, so
-- "Last 30 days" dropped what deleted conversations had cost. Like
-- provider_id, the column is now nulled instead.
ALTER TABLE model_call_usage ALTER COLUMN conversation_id DROP NOT NULL;
ALTER TABLE model_call_usage DROP CONSTRAINT model_call_usage_conversation_id_fkey;
ALTER TABLE model_call_usage
    ADD CONSTRAINT model_call_usage_conversation_id_fkey
    FOREIGN KEY (conversation_id) REFERENCES conversations (id) ON DELETE SET NULL;
