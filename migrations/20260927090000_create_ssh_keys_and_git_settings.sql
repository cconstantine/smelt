-- SSH keys and the commit identity every sandbox pod gets (SME-32).
--
-- private_key is stored as plain text for now; SME-48 is about encrypting
-- it at rest and keeping it out of the model's reach.

CREATE TABLE ssh_keys (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    public_key  TEXT NOT NULL,
    private_key TEXT NOT NULL,
    created_at  TIMESTAMP NOT NULL DEFAULT now()
);

-- One row at most: the name and email commits are made with.
CREATE TABLE git_settings (
    id          BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
    author_name  TEXT NOT NULL,
    author_email TEXT NOT NULL,
    updated_at  TIMESTAMP NOT NULL DEFAULT now()
);
