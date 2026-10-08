-- This database's identity in the cluster (SME-115). Every smelt server
-- shares the sandbox namespace and names its pods and claims after this
-- database's ids, so each cluster object is labelled smelt/instance=<id>,
-- and a server only deletes, reuses, mounts or watches its own.
--
-- owns_unlabelled is decided once, here: a database that already has
-- conversations or volumes (the dev or production database) made the
-- unlabelled objects from before this fix, and adopts them at startup. A
-- scratch, test or fresh database migrates empty and never does.

CREATE TABLE smelt_instance (
    id              BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
    instance_id     UUID NOT NULL DEFAULT gen_random_uuid(),
    owns_unlabelled BOOLEAN NOT NULL,
    created_at      TIMESTAMP NOT NULL DEFAULT now()
);

INSERT INTO smelt_instance (owns_unlabelled)
VALUES (EXISTS (SELECT 1 FROM conversations) OR EXISTS (SELECT 1 FROM sandbox_volumes));
