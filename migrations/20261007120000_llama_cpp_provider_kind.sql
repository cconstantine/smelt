-- A llama.cpp provider kind (SME-111). Its turns carry the chat template's
-- own settings (chat_template_kwargs), sent only when its /props says the
-- template reads them. Existing providers keep their kind.
ALTER TABLE inference_providers DROP CONSTRAINT inference_providers_kind_check;
ALTER TABLE inference_providers ADD CONSTRAINT inference_providers_kind_check
    CHECK (kind IN ('anthropic', 'ollama', 'llama_cpp', 'other'));

-- keep_reasoning: send preserve_thinking, so the template renders earlier
-- turns' reasoning (on, as llama.cpp templates did before). server_caps:
-- the server's /props as providers::LlamaServerInfo, written by its model
-- listing; null until first read.
ALTER TABLE inference_providers
    ADD COLUMN keep_reasoning BOOLEAN NOT NULL DEFAULT true,
    ADD COLUMN server_caps JSONB;
