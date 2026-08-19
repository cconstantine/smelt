-- Generic, purpose-agnostic mount points for sandbox pods — the user
-- names one, gives it a mount path, and every sandbox pod smelt creates
-- gets it mounted there. See
-- docs/projects/plans/sandbox-native-environment.md's Phase 4.
--
-- Plain CRUD, no soft delete: configuration a person edits, same
-- precedent as mcp_servers, not a live external resource with its own
-- lifecycle to preserve history for.
--
-- No `kind` column yet — every volume this pass is a directory, backed by
-- a PersistentVolumeClaim. A single-file (Secret-backed) kind arrives in a
-- later pass alongside upload support (see the plan's "What").
--
-- mount_path is set once at creation and never changed after (same
-- "delete and recreate to change" precedent mcp_servers.auth_mode already
-- established) — it's load-bearing for the volumeMounts entry every pod
-- gets, and a leading `~` in what the user typed is already expanded to
-- the sandbox user's home directory before this row is ever written, so
-- what's stored here is always a plain absolute path.

CREATE TABLE sandbox_volumes (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    mount_path  TEXT NOT NULL,
    created_at  TIMESTAMP NOT NULL DEFAULT now(),
    updated_at  TIMESTAMP NOT NULL DEFAULT now()
);
