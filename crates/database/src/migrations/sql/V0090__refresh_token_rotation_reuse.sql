-- V0090 adds nullable columns compatible with replicas running the previous
-- refresh-token implementation during a rolling deployment. The migration
-- runner builds the lookup index concurrently after this transaction commits.
-- Fail fast rather than queue authentication writes behind this DDL if another
-- transaction holds the table.
SET LOCAL lock_timeout = '5s';

ALTER TABLE refresh_tokens
    ADD COLUMN previous_token_hash VARCHAR(64),
    ADD COLUMN rotated_at TIMESTAMPTZ;
