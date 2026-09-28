-- The git repositories a conversation works on (SME-32). smelt clones each
-- into /workspace/<dir> of the conversation's pod, and again into every
-- new pod the conversation gets.
--
-- remote_key is the URL's normalised identity (git::remote_key), what
-- trust decisions are remembered by. branch is what was asked for (NULL:
-- the remote's default); checked_out_branch and commit_sha are what the
-- last clone got.

CREATE TABLE conversation_repos (
    id                 BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    conversation_id    BIGINT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    url                TEXT NOT NULL,
    remote_key         TEXT NOT NULL,
    branch             TEXT,
    dir                TEXT NOT NULL,
    status             TEXT NOT NULL CHECK (status IN ('cloning', 'ready', 'failed')),
    error              TEXT,
    checked_out_branch TEXT,
    commit_sha         TEXT,
    created_at         TIMESTAMP NOT NULL DEFAULT now(),
    updated_at         TIMESTAMP NOT NULL DEFAULT now(),
    UNIQUE (conversation_id, dir)
);
