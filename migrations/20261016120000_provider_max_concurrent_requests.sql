-- SME-162: how many requests a provider is sent at once. NULL is no
-- limit (every existing provider); 0 is the slot count its server
-- reports (a llama.cpp server's `/props` total_slots); n > 0 is that many.
ALTER TABLE inference_providers
    ADD COLUMN max_concurrent_requests INTEGER
        CHECK (max_concurrent_requests IS NULL OR max_concurrent_requests >= 0);
