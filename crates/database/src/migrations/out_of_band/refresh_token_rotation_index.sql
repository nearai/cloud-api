-- Run outside a transaction after V0088 and before deploying the new API code.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_refresh_tokens_previous_hash
    ON refresh_tokens(previous_token_hash)
    WHERE previous_token_hash IS NOT NULL;

-- A failed concurrent build can leave an INVALID index that IF NOT EXISTS
-- will skip on retry. Fail the script instead of printing false and exiting 0.
-- Drop the invalid index and rerun this script before starting the new API.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index AS i
        WHERE i.indexrelid = to_regclass('idx_refresh_tokens_previous_hash')
          AND i.indrelid = 'refresh_tokens'::regclass
          AND i.indisvalid
          AND i.indisready
    ) THEN
        RAISE EXCEPTION 'idx_refresh_tokens_previous_hash is missing, invalid, or unready';
    END IF;
END $$;
