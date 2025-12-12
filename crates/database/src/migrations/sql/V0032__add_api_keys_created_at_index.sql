-- Add index to support incremental Bloom filter sync queries.
-- The sync query filters on:
--   created_at > $1 AND is_active = true AND deleted_at IS NULL
--
-- Partial index keeps it smaller and focused on the active-key subset.
CREATE INDEX IF NOT EXISTS idx_api_keys_active_created_at
    ON api_keys (created_at)
    WHERE is_active = true AND deleted_at IS NULL;

