-- Run outside a transaction after V0088 and before deploying the new API code.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_refresh_tokens_previous_hash
    ON refresh_tokens(previous_token_hash)
    WHERE previous_token_hash IS NOT NULL;

-- Must return true; a failed concurrent build can leave an invalid index.
SELECT i.indisvalid
FROM pg_index AS i
WHERE i.indexrelid = 'idx_refresh_tokens_previous_hash'::regclass;
