-- A repo's AGENTS.md as loaded into the model's context (SME-32): the
-- first 32 KiB of it (instructions), the whole file's size, the sha256 of
-- what was read (to notice it changing), the commit it was loaded at, and
-- the repo's other AGENTS.md files. instructions is NULL when nothing is
-- loaded: the repo has none, or it hasn't been trusted.

ALTER TABLE conversation_repos
    ADD COLUMN instructions        TEXT,
    ADD COLUMN instructions_bytes  BIGINT,
    ADD COLUMN instructions_hash   TEXT,
    ADD COLUMN instructions_commit TEXT,
    ADD COLUMN nested_instructions TEXT[] NOT NULL DEFAULT '{}';
