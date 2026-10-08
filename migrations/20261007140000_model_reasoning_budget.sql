-- A model's reasoning budget (SME-111): the user's cap on how many tokens
-- a reply may spend thinking, sent as thinking.budget_tokens to a
-- llama.cpp provider only, which forces the end of thinking there. Null:
-- three quarters of the reply budget.
ALTER TABLE provider_models
    ADD COLUMN reasoning_budget INTEGER CHECK (reasoning_budget > 0);
