-- SME-235: a checkout's persona files, recorded at clone as `agents_files`
-- is: the tracked Markdown files directly in `.agents/personas/`, relative
-- to the checkout, sorted. Paths only: nothing from inside the files is
-- stored, so an untrusted repo's content never reaches a model-visible row.
-- `list_personas` re-records it from the checkout on each call. Existing
-- rows start empty: the migration can't look in the pods, so a repo cloned
-- before this shows none in the system prompt until `list_personas` runs.
ALTER TABLE conversation_repos
    ADD COLUMN persona_files TEXT[] NOT NULL DEFAULT '{}';
