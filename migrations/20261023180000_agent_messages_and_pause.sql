-- SME-238: agents message each other, a child asks its parent, and the
-- pause after a Stop is stored.
--
-- `agent_messages.kind` gains 'message' (`message_agent`) and 'question'
-- (`ask_parent`'s question, queued to the parent). The constraint is the
-- unnamed column CHECK of 20261023120000_child_agents.sql, which Postgres
-- names `<table>_<column>_check`.
ALTER TABLE agent_messages DROP CONSTRAINT agent_messages_kind_check;
ALTER TABLE agent_messages
    ADD CONSTRAINT agent_messages_kind_check CHECK (kind IN ('report', 'notice', 'message', 'question'));

-- Who a waiting question is for: the user (`ask_user`, the card) or the
-- child's parent (`ask_parent`, answered by its next `message_agent`).
-- Every reader that means "the user is asked" filters on 'user'.
ALTER TABLE pending_questions
    ADD COLUMN asked_of TEXT NOT NULL DEFAULT 'user' CHECK (asked_of IN ('user', 'parent'));

-- When the user (or a parent's `stop_agent`) stopped the conversation and
-- nobody has written in it since: the in-memory pause, mirrored so it
-- survives a restart. Null when not paused.
ALTER TABLE conversations ADD COLUMN paused_at TIMESTAMP;
