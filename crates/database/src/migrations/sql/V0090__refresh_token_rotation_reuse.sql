-- V0090 adds nullable columns compatible with replicas running the previous
-- refresh-token implementation during a rolling deployment. IF NOT EXISTS
-- also tolerates databases that ran an earlier provisional version of this
-- branch migration. The migration runner builds the lookup index concurrently
-- after this transaction commits.
-- Fail fast rather than queue authentication writes behind this DDL if another
-- transaction holds the table.
SET LOCAL lock_timeout = '5s';

ALTER TABLE refresh_tokens
    ADD COLUMN IF NOT EXISTS previous_token_hash VARCHAR(64),
    ADD COLUMN IF NOT EXISTS rotated_at TIMESTAMPTZ;
