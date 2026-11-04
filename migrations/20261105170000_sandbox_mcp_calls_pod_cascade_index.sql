-- SME-386 M5: a pod's delete cascades to its calls, and the cascade's own
-- delete (`WHERE pod_id = $1`) can't use the partial index on command-less
-- calls, so it scanned every call. A plain index on `pod_id` serves it.
-- The table is not pruned: its rows go with their pod or conversation.
CREATE INDEX idx_sandbox_mcp_calls_pod ON sandbox_mcp_calls(pod_id);
