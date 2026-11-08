-- SME-450: a conversation whose turn saved something for the model that
-- the model hasn't answered yet, set in the transaction that saves it and
-- cleared by the reply's and at the turn's end. Set with no turn running,
-- a restart (or a panic) dropped the turn: the startup sweep wakes it.
ALTER TABLE conversations ADD COLUMN answer_owed_at TIMESTAMP NULL;

-- 'server_notice': a `Notice::Deliver` queued for its wake, its body the
-- already-marked text, saved verbatim, so it survives a restart.
ALTER TABLE agent_messages DROP CONSTRAINT agent_messages_kind_check;
ALTER TABLE agent_messages ADD CONSTRAINT agent_messages_kind_check
    CHECK (kind IN ('report', 'notice', 'message', 'question', 'server_notice'));
