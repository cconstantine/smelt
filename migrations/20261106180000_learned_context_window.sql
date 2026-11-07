-- SME-439: a context window smelt learned is kept apart from the user's
-- own Context window, so learning can never overwrite what the user set.
-- `learned_context_window` is what a turn believes when the user's
-- `context_window` is unset (read order user → learned → reported);
-- `_at`, `_reason` and `_conversation` record when each learning happened,
-- which learner wrote it and from which conversation — written at save
-- time, since nothing else can recover them later. The conversation id
-- carries no FK: it must still read right when the conversation is gone,
-- and deleting the provider cascades the row itself.
ALTER TABLE provider_models
    ADD COLUMN learned_context_window integer,
    ADD COLUMN learned_context_window_at timestamptz,
    ADD COLUMN learned_context_window_reason text,
    ADD COLUMN learned_context_window_conversation bigint;
