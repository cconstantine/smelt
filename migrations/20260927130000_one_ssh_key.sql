-- One SSH key at a time, for now (SME-32 code review 3): with several, the
-- git host takes the first key it knows, so per-repo deploy keys got
-- "Repository not found" for every repo but one. An expression index on a
-- constant allows one row.
CREATE UNIQUE INDEX ssh_keys_only_one ON ssh_keys ((true));
