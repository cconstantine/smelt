-- SME-484: what a conversation's sandbox pods ask of their node, a
-- persona's `smelt.resources` as JSON (`sandbox::PodResources`). NULL is
-- the deployment's defaults: a request of 0 and its limits. Every pod
-- start reads it.
ALTER TABLE conversations ADD COLUMN pod_resources JSONB NULL;
