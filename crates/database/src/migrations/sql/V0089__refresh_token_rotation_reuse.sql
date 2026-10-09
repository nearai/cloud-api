-- These nullable columns remain compatible with replicas running the previous
-- refresh-token implementation during a rolling deployment. Build the lookup
-- index in the same startup migration so no separate rollout step is required.
-- Fail fast rather than queue authentication writes behind this DDL if another
-- transaction holds the table.
SET LOCAL lock_timeout = '5s';

ALTER TABLE refresh_tokens
    ADD COLUMN previous_token_hash VARCHAR(64),
    ADD COLUMN rotated_at TIMESTAMPTZ;

CREATE INDEX idx_refresh_tokens_previous_hash
    ON refresh_tokens(previous_token_hash)
    WHERE previous_token_hash IS NOT NULL;
