-- A model's output cap (SME-111): the most a reply may be. max_output is
-- the user's ("Max reply tokens"); reported_max_output what the provider
-- last reported (Anthropic's model listing's max_tokens), kept when a
-- report lacks one. A turn's reply budget is capped by the first set.
ALTER TABLE provider_models
    ADD COLUMN max_output INTEGER CHECK (max_output > 0),
    ADD COLUMN reported_max_output INTEGER;
