-- Which attempt at a clone is current (SME-86): a retry increments it, and
-- only the current attempt may finish the clone, so a stale attempt's late
-- "ready" or "failed" can't overwrite a newer one.
ALTER TABLE conversation_repos ADD COLUMN attempt INTEGER NOT NULL DEFAULT 1;
