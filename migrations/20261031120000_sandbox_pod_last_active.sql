-- SME-78: the last activity seen on a pod outside its database records — a
-- preview request, a pod dial, the end of a turn that brought something new.
-- Noted in memory and flushed here at most once a minute per conversation, so
-- the idle clock survives a restart. NULL until the first flush.
ALTER TABLE sandbox_pods ADD COLUMN last_active_at TIMESTAMP NULL;
