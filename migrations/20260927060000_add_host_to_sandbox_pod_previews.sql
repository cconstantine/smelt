-- A preview can reach a Docker container in the pod as well as the pod's
-- own localhost (SME-33): '' is localhost, otherwise the container's
-- address. A container's port is a preview of its own, next to the same
-- port on localhost.
ALTER TABLE sandbox_pod_previews ADD COLUMN host TEXT NOT NULL DEFAULT '';
ALTER TABLE sandbox_pod_previews DROP CONSTRAINT sandbox_pod_previews_pkey;
ALTER TABLE sandbox_pod_previews ADD PRIMARY KEY (pod_id, host, port);
