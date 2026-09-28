-- AGENTS.md is loaded by the model, through the load_instructions tool,
-- rather than automatically on clone (SME-32 code review 4, at the user's
-- request). Each loaded file is a row of its own (any AGENTS.md in a
-- checkout, nested ones included), and a load waiting on the user's trust
-- decision keeps the exact file the trust card shows, so Trust loads what
-- the user read and nothing else.

ALTER TABLE conversation_repos
    DROP COLUMN instructions,
    DROP COLUMN instructions_bytes,
    DROP COLUMN instructions_hash,
    DROP COLUMN instructions_commit,
    DROP COLUMN nested_instructions,
    DROP COLUMN found_instructions,
    DROP COLUMN found_bytes,
    DROP COLUMN found_hash,
    DROP COLUMN found_commit,
    DROP COLUMN found_nested,
    -- The checkout's AGENTS.md files, relative to it, top-level first: what
    -- the model can load.
    ADD COLUMN agents_files TEXT[] NOT NULL DEFAULT '{}';

-- In the model's context, in the system prompt's Project instructions.
CREATE TABLE loaded_instructions (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    conversation_id BIGINT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    repo_id         BIGINT NOT NULL REFERENCES conversation_repos(id) ON DELETE CASCADE,
    -- Relative to the checkout, e.g. AGENTS.md or web/AGENTS.md.
    path            TEXT NOT NULL,
    -- At most 32 KiB of it; file_bytes is the whole file's size.
    content         TEXT NOT NULL,
    file_bytes      BIGINT NOT NULL,
    hash            TEXT NOT NULL,
    commit_sha      TEXT,
    loaded_at       TIMESTAMP NOT NULL DEFAULT now(),
    UNIQUE (repo_id, path)
);

-- Waiting on the user's trust decision about the repo's remote.
CREATE TABLE instruction_requests (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    conversation_id BIGINT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    repo_id         BIGINT NOT NULL REFERENCES conversation_repos(id) ON DELETE CASCADE,
    path            TEXT NOT NULL,
    content         TEXT NOT NULL,
    file_bytes      BIGINT NOT NULL,
    hash            TEXT NOT NULL,
    commit_sha      TEXT,
    created_at      TIMESTAMP NOT NULL DEFAULT now(),
    UNIQUE (repo_id, path)
);
