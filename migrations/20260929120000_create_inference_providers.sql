-- Model providers, configured on /providers instead of the ANTHROPIC_*
-- environment variables (SME-72). Every provider is an Anthropic-compatible
-- /v1/messages endpoint; `kind` only decides how its models are listed and
-- described.
--
-- secret is stored as plain text for now, like ssh_keys.private_key;
-- SME-48 is about encrypting secrets at rest.

CREATE TABLE inference_providers (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    kind        TEXT NOT NULL CHECK (kind IN ('anthropic', 'ollama', 'other')),
    base_url    TEXT NOT NULL,
    -- api_key: an `x-api-key` header; bearer: `Authorization: Bearer`.
    auth_kind   TEXT NOT NULL CHECK (auth_kind IN ('api_key', 'bearer')),
    secret      TEXT NOT NULL,
    created_at  TIMESTAMP NOT NULL DEFAULT now(),
    updated_at  TIMESTAMP NOT NULL DEFAULT now()
);

-- What's known about a provider's models: the user's overrides, which win,
-- and what the provider last reported. Null means unset or not reported.
CREATE TABLE provider_models (
    provider_id              BIGINT NOT NULL REFERENCES inference_providers (id) ON DELETE CASCADE,
    model                    TEXT NOT NULL,
    thinking                 BOOLEAN,
    context_window           INTEGER CHECK (context_window > 0),
    reported_context_window  INTEGER,
    reported_thinking        BOOLEAN,
    reported_tools           BOOLEAN,
    -- Added as a model the listing doesn't show: listed even when the
    -- listing works and lacks it. Other rows are shown only while the
    -- listing has them (or they have an override), so a model the provider
    -- dropped goes away.
    added_by_hand            BOOLEAN NOT NULL DEFAULT false,
    PRIMARY KEY (provider_id, model)
);

-- One row at most: the provider and model a conversation takes when its
-- next turn starts without one. RESTRICT, not SET NULL: delete_provider
-- clears it in the same transaction, and a bug there should fail the
-- delete rather than leave a model with no provider.
CREATE TABLE inference_settings (
    id                   BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
    default_provider_id  BIGINT REFERENCES inference_providers (id) ON DELETE RESTRICT,
    default_model        TEXT,
    CHECK ((default_provider_id IS NULL) = (default_model IS NULL))
);

-- A conversation's own provider and model; null until its first turn takes
-- the default. last_turn_model_key: the backend its last turn ran on
-- (provider, base URL, model), and model_changed_at_message_id its last
-- message when a turn started on a different one, so older thinking blocks
-- (signed by another backend) aren't replayed. Both are set when a turn
-- starts, under the turn lock, not when the model is picked: a turn still
-- running keeps writing for the old model.
ALTER TABLE conversations
    ADD COLUMN provider_id BIGINT REFERENCES inference_providers (id) ON DELETE RESTRICT,
    ADD COLUMN model TEXT,
    ADD COLUMN last_turn_model_key TEXT,
    ADD COLUMN model_changed_at_message_id BIGINT,
    ADD CONSTRAINT conversations_provider_and_model_together
        CHECK ((provider_id IS NULL) = (model IS NULL));
