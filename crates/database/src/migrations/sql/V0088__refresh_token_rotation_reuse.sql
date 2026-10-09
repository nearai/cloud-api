-- Deploy this additive schema change before the new refresh code.
-- Build the previous-token index separately and concurrently before enabling
-- the new code, to avoid blocking refresh-token writes during API startup.
-- Refinery runs each migration in a transaction. Fail fast rather than queue
-- authentication writes behind this DDL if another transaction holds the table.
SET LOCAL lock_timeout = '5s';

ALTER TABLE refresh_tokens
    ADD COLUMN previous_token_hash VARCHAR(64),
    ADD COLUMN rotated_at TIMESTAMPTZ;
