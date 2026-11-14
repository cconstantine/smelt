-- SME-523: a Claude Code conversation's prompt-cache re-seed is a model
-- call of its own kind, so its tokens count and can be told apart.
ALTER TABLE model_call_usage DROP CONSTRAINT model_call_usage_kind_check;
ALTER TABLE model_call_usage ADD CONSTRAINT model_call_usage_kind_check
    CHECK (kind IN ('turn', 'compaction', 'reseed'));
