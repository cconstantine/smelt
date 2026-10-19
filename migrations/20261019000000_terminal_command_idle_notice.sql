-- SME-79: an idle notice when a terminal command goes untouched. smelt
-- never stops a command on a timer. When one has gone `idle_notice_secs`
-- without anything touching it, smelt wakes the conversation and tells the
-- model, which decides what to do.
--
-- `idle_notice_secs` is stored when the command starts — the deployment
-- default (`SANDBOX_IDLE_NOTICE_MINUTES`, 5 minutes) or the value
-- `run_terminal_command` was given, `0` for never — so a running command
-- keeps the interval it started with when the default later changes.
-- `touched_at` is the last time the model touched the command (checked its
-- status, read its output, signalled it, re-timed it); NULL means never, so
-- the clock counts from `created_at`. Output is not a touch.
-- `idle_notice_at` marks the last notice sent, so the clock restarts after
-- each one; there is no cap on how many a command gets.
--
-- The due rule itself lives in db.rs as one SQL fragment, over
-- GREATEST(created_at, touched_at, idle_notice_at), so the watch and the
-- drain can't disagree.
--
-- Existing rows get the DDL default of 300 s and a NULL `touched_at`, so a
-- command already running when this migration runs can be noticed five
-- minutes later. Harmless: on a dev server those commands are minutes old at
-- worst, and the notice only tells the model something is still running.
ALTER TABLE terminal_commands
    ADD COLUMN idle_notice_secs INTEGER NOT NULL DEFAULT 300 CHECK (idle_notice_secs >= 0),
    ADD COLUMN touched_at TIMESTAMP,
    ADD COLUMN idle_notice_at TIMESTAMP;

-- The watch's pass reads only running, notice-enabled commands, so it stays
-- cheap however long the table has been accumulating finished ones.
CREATE INDEX idx_terminal_commands_running_idle
    ON terminal_commands(conversation_id, created_at)
    WHERE status = 'running' AND idle_notice_secs > 0;
