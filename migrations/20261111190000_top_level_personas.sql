-- SME-481: a top-level conversation set to a persona once, after its repo
-- is attached and before its first message (docs/design/personas/top-level.md).
-- The snapshot is a child's (`agents.persona_*`), the chain is the
-- persona's one `smelt.tools` entry, and `credentials` is Rule G's bit.
-- `applied_at` stays NULL until the pod matches the grant; turns are
-- refused meanwhile, and the startup sweep deletes an unapplied row.
CREATE TABLE top_level_personas (
    conversation_id BIGINT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
    persona_name TEXT NOT NULL,
    persona_path TEXT NOT NULL,
    persona_commit TEXT,
    persona_body TEXT NOT NULL,
    persona_settings JSONB NOT NULL,
    rule_chain JSONB,
    credentials TEXT[] NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT now(),
    applied_at TIMESTAMP
);
