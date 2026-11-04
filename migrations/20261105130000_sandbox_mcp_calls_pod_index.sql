-- SME-359 review round 1, M2: the budget for a pod's calls traced to no
-- command counts them on every call; this keeps that count off a scan of
-- every call the pod's scripts made.
CREATE INDEX idx_sandbox_mcp_calls_pod_without_command
    ON sandbox_mcp_calls(pod_id) WHERE command_id IS NULL;
