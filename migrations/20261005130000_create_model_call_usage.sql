-- One row per completed model call (SME-106): what it used and, when the
-- price catalog knew its model, what it cost. Append-only. Unlike
-- conversation_context_usage (the last call only, for the context
-- indicator), this is the history the cost display sums.
--
-- provider_id is SET NULL when a provider is deleted, keeping the model
-- and tokens. cost_usd is fixed when the row is written, from the prices
-- in force then; null when the catalog had no price for the call.
CREATE TABLE model_call_usage (
    id                           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    conversation_id              BIGINT NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    provider_id                  BIGINT REFERENCES inference_providers (id) ON DELETE SET NULL,
    model                        TEXT NOT NULL,
    kind                         TEXT NOT NULL CHECK (kind IN ('turn', 'compaction')),
    input_tokens                 BIGINT NOT NULL,
    output_tokens                BIGINT NOT NULL,
    cache_creation_input_tokens  BIGINT NOT NULL,
    cache_read_input_tokens      BIGINT NOT NULL,
    cost_usd                     DOUBLE PRECISION,
    created_at                   TIMESTAMP NOT NULL DEFAULT now()
);

CREATE INDEX model_call_usage_conversation ON model_call_usage (conversation_id);
CREATE INDEX model_call_usage_created_at ON model_call_usage (created_at);
