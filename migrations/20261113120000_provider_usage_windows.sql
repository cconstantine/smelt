-- SME-486: the Claude subscription's usage windows, as the CLI's
-- rate_limit_event last said them, per provider and window
-- (`five_hour`, `seven_day`, …). Only numbers, a checked name and a
-- status from a fixed list: no column could hold a session id or token.
-- A row is replaced only by a reading at least as new (`seen_at`).
CREATE TABLE provider_usage_windows (
    provider_id BIGINT NOT NULL REFERENCES inference_providers(id) ON DELETE CASCADE,
    window_name TEXT NOT NULL,
    utilization DOUBLE PRECISION NOT NULL,
    status      TEXT NOT NULL,
    resets_at   TIMESTAMPTZ NULL,
    seen_at     TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (provider_id, window_name)
);
