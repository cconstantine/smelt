-- N pods and N terminals per conversation (was: at most one of each,
-- identity implicit in conversation_id). See
-- docs/projects/plans/sandbox-terminal.md.

CREATE TABLE sandbox_pods (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    conversation_id BIGINT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    created_at      TIMESTAMP NOT NULL DEFAULT now(),
    -- Soft-delete: terminate_pod sets this rather than DELETEing the row,
    -- so a hard delete never cascades into (and destroys) the command
    -- history that ran under it. See the plan's "How".
    terminated_at   TIMESTAMP
);

CREATE INDEX idx_sandbox_pods_conversation_id ON sandbox_pods(conversation_id);
CREATE INDEX idx_sandbox_pods_conversation_live
    ON sandbox_pods(conversation_id) WHERE terminated_at IS NULL;

CREATE TABLE sandbox_terminals (
    id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    pod_id        BIGINT NOT NULL REFERENCES sandbox_pods(id) ON DELETE CASCADE,
    created_at    TIMESTAMP NOT NULL DEFAULT now(),
    -- Same soft-delete reasoning as sandbox_pods.terminated_at above.
    terminated_at TIMESTAMP
);

CREATE INDEX idx_sandbox_terminals_pod_id ON sandbox_terminals(pod_id);
CREATE INDEX idx_sandbox_terminals_pod_live
    ON sandbox_terminals(pod_id) WHERE terminated_at IS NULL;

-- Pre-dates this migration: rows from before terminal_id existed have
-- nothing valid to backfill against (there was never a persisted terminal
-- identity), so they're cleared rather than kept orphaned. terminal_events
-- cascades with them.
DELETE FROM terminal_commands;

ALTER TABLE terminal_commands
    ADD COLUMN terminal_id BIGINT NOT NULL REFERENCES sandbox_terminals(id) ON DELETE CASCADE;

-- The single-command-in-flight guard and list_commands both move from
-- conversation_id to terminal_id scope.
DROP INDEX idx_terminal_commands_conversation_running;
CREATE INDEX idx_terminal_commands_terminal_running
    ON terminal_commands(terminal_id) WHERE status = 'running';

DROP INDEX idx_terminal_commands_conversation_id;
CREATE INDEX idx_terminal_commands_terminal_id ON terminal_commands(terminal_id, id);
