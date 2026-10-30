-- SME-242: the user's named secrets. Plain text for now; SME-48 is
-- about encrypting secrets at rest (the same note `ssh_keys` and
-- `inference_providers` carry).
--
-- `name` is what a persona lists in `smelt.credentials`, immutable, and
-- a file name in a pod — hence the `validate_key_name` shape. The
-- `git-ssh` refusal belongs here rather than in the persona parser:
-- Rule G's built-in keeps its one meaning, and a caller that skips the
-- API's validation still can't shadow it (T3).

CREATE TABLE named_secrets (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE CHECK (name ~ '^[A-Za-z0-9_-]{1,64}$' AND name <> 'git-ssh'),
    value       TEXT NOT NULL CHECK (value <> ''),
    created_at  TIMESTAMP NOT NULL DEFAULT now(),
    updated_at  TIMESTAMP NOT NULL DEFAULT now()
);
