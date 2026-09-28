-- SME-35: language servers the user has configured, as data. A server's
-- pods aren't recorded here: Kubernetes is their record (found by label).
CREATE TABLE language_servers (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    image TEXT NOT NULL,
    install_command TEXT NOT NULL DEFAULT '',
    command TEXT NOT NULL,
    args JSONB NOT NULL DEFAULT '[]',
    env JSONB NOT NULL DEFAULT '{}',
    file_types JSONB NOT NULL DEFAULT '{}',
    root_markers JSONB NOT NULL DEFAULT '[]',
    initialization_options JSONB,
    settings JSONB,
    memory_limit TEXT NOT NULL,
    cpu_limit TEXT NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMP NOT NULL DEFAULT now(),
    updated_at TIMESTAMP NOT NULL DEFAULT now()
);
