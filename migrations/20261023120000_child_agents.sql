-- SME-236: child agents. A child is a whole conversation with a parent
-- (`parent_id`, cascading, so the database never holds a child whose parent
-- is gone), an `agents` row holding its lifecycle and persona snapshot, and
-- a durable queue, `agent_messages`, through which reports and notices
-- reach a conversation's turn loop (docs/design/personas/agents.md).
--
-- `deleting_at` is set by a delete under the group root's lock, so
-- `start_agent` can't add an agent to a subtree being deleted.
ALTER TABLE conversations
    ADD COLUMN parent_id BIGINT REFERENCES conversations (id) ON DELETE CASCADE,
    ADD COLUMN deleting_at TIMESTAMP;
CREATE INDEX conversations_parent_id ON conversations (parent_id) WHERE parent_id IS NOT NULL;

-- One row per child conversation. `state` is the only stored part of an
-- agent's status; every write of it is `UPDATE ... WHERE state = <expected>`.
-- `started_at` is set when the start completes: a child stopped during its
-- start is `stopped` with no `started_at`, and can never become `active`.
-- The persona columns are a snapshot, all null without one. SME-241 and
-- SME-242 add `rule_chain` and `credentials`.
CREATE TABLE agents (
    conversation_id  BIGINT PRIMARY KEY REFERENCES conversations (id) ON DELETE CASCADE,
    root_id          BIGINT NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    depth            INT NOT NULL CHECK (depth >= 1),
    brief            TEXT NOT NULL,
    persona_name     TEXT,
    persona_path     TEXT,
    persona_commit   TEXT,
    persona_body     TEXT,
    persona_settings JSONB,
    state            TEXT NOT NULL CHECK (state IN ('starting', 'active', 'finished', 'stopped', 'failed')),
    created_at       TIMESTAMP NOT NULL DEFAULT now(),
    state_changed_at TIMESTAMP NOT NULL DEFAULT now(),
    started_at       TIMESTAMP
);
CREATE INDEX agents_root_id ON agents (root_id);

-- The queue: each row becomes one `user` message in its recipient's
-- transcript, saved and marked delivered in one transaction under the
-- recipient's turn lock. A deleted sender leaves `from_conversation_id`
-- null and its `from_label` kept. SME-238 adds the kind 'message'.
CREATE TABLE agent_messages (
    id                   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    to_conversation_id   BIGINT NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    from_conversation_id BIGINT REFERENCES conversations (id) ON DELETE SET NULL,
    from_label           TEXT NOT NULL,
    kind                 TEXT NOT NULL CHECK (kind IN ('report', 'notice')),
    body                 TEXT NOT NULL,
    created_at           TIMESTAMP NOT NULL DEFAULT now(),
    delivered_at         TIMESTAMP,
    delivered_message_id BIGINT REFERENCES messages (id) ON DELETE SET NULL
);
CREATE INDEX agent_messages_undelivered ON agent_messages (to_conversation_id, id) WHERE delivered_at IS NULL;
