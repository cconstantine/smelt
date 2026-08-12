CREATE TABLE terminal_commands (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    conversation_id BIGINT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    command_id      TEXT NOT NULL,
    command         TEXT NOT NULL,
    status          TEXT NOT NULL CHECK (status IN ('running', 'finished', 'lost')),
    exit_code       INTEGER,
    notified_at     TIMESTAMP,
    created_at      TIMESTAMP NOT NULL DEFAULT now(),
    finished_at     TIMESTAMP
);

CREATE UNIQUE INDEX idx_terminal_commands_command_id ON terminal_commands(command_id);
CREATE INDEX idx_terminal_commands_conversation_running
    ON terminal_commands(conversation_id) WHERE status = 'running';
CREATE INDEX idx_terminal_commands_conversation_id ON terminal_commands(conversation_id, id);

CREATE TABLE terminal_events (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    command_id  TEXT NOT NULL REFERENCES terminal_commands(command_id) ON DELETE CASCADE,
    stream      TEXT NOT NULL CHECK (stream IN ('stdout', 'stderr')),
    seq         BIGINT NOT NULL,
    data        TEXT NOT NULL,
    created_at  TIMESTAMP NOT NULL DEFAULT now()
);

CREATE INDEX idx_terminal_events_command_id ON terminal_events(command_id, seq);
