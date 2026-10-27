-- SME-239: volumes the model creates for its own and its children's pods
-- (docs/design/personas/checkouts-and-volumes.md#volumes). One row per
-- volume, owned by the conversation that created it; a volume reaches a pod
-- through a mount row, so the owner gets one too and passing one to a child
-- adds a row rather than copying the volume. `ON DELETE CASCADE` from
-- conversations is what deletes a volume with its creator; the startup sweep
-- (src/sandbox/claims.rs) catches a claim its cut-off delete left behind.
-- Names are unique per creator — the `create_volume` tool refuses a name
-- already taken by anything the conversation mounts, so passing by name
-- stays unambiguous (the plan's first Decision).

CREATE TABLE agent_volumes (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_conversation_id BIGINT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (name ~ '^[A-Za-z0-9_-]{1,64}$'),
    mount_path TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT now(),
    UNIQUE (owner_conversation_id, name)
);

CREATE TABLE conversation_volume_mounts (
    conversation_id BIGINT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    volume_id BIGINT NOT NULL REFERENCES agent_volumes(id) ON DELETE CASCADE,
    PRIMARY KEY (conversation_id, volume_id)
);
