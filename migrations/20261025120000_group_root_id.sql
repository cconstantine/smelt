-- SME-244: cost groups. Each model call carries the id of the conversation
-- its spend belongs to: itself, or for an agent the conversation at its
-- group's root (`agents.root_id`, set at the start, so the group query
-- needs no recursion). No foreign key by design: a deleted child's spend
-- must keep counting in its group while `conversation_id` SET NULLs
-- (docs/design/personas/agents.md#limits-sme-245, the Budget row).
ALTER TABLE model_call_usage ADD COLUMN group_root_id BIGINT;

CREATE INDEX model_call_usage_group_root ON model_call_usage (group_root_id);

-- Rows predating this migration: an agent's rows to its root (agents exist
-- since SME-236), every other conversation's to itself. Rows whose
-- conversation was already deleted keep no group, as they join no
-- conversation's spend today.
UPDATE model_call_usage m
   SET group_root_id = COALESCE(
       (SELECT a.root_id FROM agents a WHERE a.conversation_id = m.conversation_id),
       m.conversation_id)
 WHERE m.group_root_id IS NULL AND m.conversation_id IS NOT NULL;
