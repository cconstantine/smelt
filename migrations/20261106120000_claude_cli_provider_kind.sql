-- A Claude Code provider kind (SME-399, SME-429): turns run through the
-- official claude CLI on the server host, signed in to the user's Claude
-- subscription. Its base_url holds the binary's path and its secret is
-- empty: it has no address or key. Existing providers keep their kind.
ALTER TABLE inference_providers DROP CONSTRAINT inference_providers_kind_check;
ALTER TABLE inference_providers ADD CONSTRAINT inference_providers_kind_check
    CHECK (kind IN ('anthropic', 'ollama', 'llama_cpp', 'other', 'claude_cli'));
