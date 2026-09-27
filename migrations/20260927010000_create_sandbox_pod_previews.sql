-- Ports in a sandbox pod the model has shared as previews for the user's
-- browser (SME-42): the sandbox panel shows a link for each. Belongs to
-- the pod, so it goes with it.
CREATE TABLE sandbox_pod_previews (
    pod_id BIGINT NOT NULL REFERENCES sandbox_pods(id) ON DELETE CASCADE,
    port INTEGER NOT NULL CHECK (port BETWEEN 1 AND 65535),
    created_at TIMESTAMP NOT NULL DEFAULT now(),
    PRIMARY KEY (pod_id, port)
);
