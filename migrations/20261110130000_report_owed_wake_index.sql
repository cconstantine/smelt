-- SME-455: the stuck predicate's wake clauses (the nudge check and the
-- tree's report-owed mark) probe for command ends recorded but never
-- drained — status IN ('finished', 'lost') AND notified_at IS NULL, the
-- `UNNOTIFIED_END` shape. `idx_terminal_commands_conversation_running`
-- is partial to status = 'running' and serves only the running-command
-- clause, `idx_terminal_commands_conversation_id` was dropped
-- (20260812041850), and terminal_commands keeps its finished rows: the
-- probe would seq-scan the table once per tree row (SME-389 R1-1's
-- EXPLAIN caught it in `test_the_running_command_reads_use_an_index`).
-- A partial index matching the clause, SME-389 M5's shape.
CREATE INDEX idx_terminal_commands_unnotified_end
    ON terminal_commands(conversation_id)
    WHERE status IN ('finished', 'lost') AND notified_at IS NULL;
