-- SME-389 review round 1 (R1-1): recreate the partial index this name's
-- creator wrote (20260811050639) and 20260812041850 dropped. B's
-- finish-time read and idle_agent_without_report's NOT EXISTS both filter
-- conversation_id = $ AND status = 'running'; neither surviving index
-- serves it -- idx_terminal_commands_running_idle can't be used at all
-- (the queries don't imply idle_notice_secs > 0) and
-- idx_terminal_commands_terminal_running has no conversation_id, so each
-- read seq-scans terminal_commands, a table that keeps its finished rows.
CREATE INDEX idx_terminal_commands_conversation_running
    ON terminal_commands(conversation_id)
    WHERE status = 'running';
