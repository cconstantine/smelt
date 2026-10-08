-- A model's effort (SME-106): sent as output_config.effort on turns run on
-- an Anthropic provider. Null sends none, leaving the model's own default
-- (medium on Claude Opus 5.5, high on most others).
ALTER TABLE provider_models
    ADD COLUMN effort TEXT CHECK (effort IN ('low', 'medium', 'high', 'xhigh', 'max'));
