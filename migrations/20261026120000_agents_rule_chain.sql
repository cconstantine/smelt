-- SME-241: the rule chain. Each agent's effective tool rules, stored at
-- its start: a JSON array of `smelt.tools` entries, oldest first — the
-- parent's chain as it stood at `create_child`, plus the agent's own
-- persona's entry when its start completed. Evaluation walks it per tool
-- offer and per call; nothing re-reads persona files, so the rules — and
-- the offered tool list — are fixed for the conversation's life
-- (docs/design/personas/tool-rules.md#narrowing). NULL is the empty
-- chain: allow everything, the user's own conversation as today.

ALTER TABLE agents ADD COLUMN rule_chain JSONB;
