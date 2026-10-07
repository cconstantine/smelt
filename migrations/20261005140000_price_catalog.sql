-- Model prices from the models.dev catalog (SME-106). One row at most: the
-- last catalog fetched, as src/pricing.rs keeps it (provider id -> name,
-- API address and each priced model's prices), so a restart without
-- network still has prices. Written only by the hourly refresh.
CREATE TABLE price_catalog (
    id          BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
    fetched_at  TIMESTAMP NOT NULL,
    providers   JSONB NOT NULL
);

-- Which catalog provider prices a smelt provider's calls; null for none (a
-- local server, or a flat plan the catalog doesn't price at $0). Existing
-- providers on Anthropic's own API are matched now; the form suggests one
-- for the rest.
ALTER TABLE inference_providers ADD COLUMN price_catalog_provider TEXT;

UPDATE inference_providers SET price_catalog_provider = 'anthropic'
WHERE kind = 'anthropic' AND base_url LIKE 'https://api.anthropic.com%';
