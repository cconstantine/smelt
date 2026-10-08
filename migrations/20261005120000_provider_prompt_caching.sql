-- Whether turns on a provider mark their requests for prompt caching
-- (SME-106). On for Anthropic, which bills a cache read at a tenth of the
-- input price; off for the other kinds, where an unknown strict server
-- could refuse the `cache_control` field. The user can change it on the
-- provider's form.
ALTER TABLE inference_providers
    ADD COLUMN prompt_caching BOOLEAN NOT NULL DEFAULT false;

UPDATE inference_providers SET prompt_caching = true WHERE kind = 'anthropic';
