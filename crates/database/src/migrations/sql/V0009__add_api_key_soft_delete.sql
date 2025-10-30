-- Add soft delete support for API keys
-- This allows distinguishing between paused (is_active = false) and deleted (deleted_at IS NOT NULL) keys

-- Add deleted_at column to api_keys table
ALTER TABLE api_keys ADD COLUMN deleted_at TIMESTAMPTZ DEFAULT NULL;

-- Add index for filtering deleted keys (partial index for performance)
-- This index only includes non-deleted keys, which is the common query case
CREATE INDEX idx_api_keys_deleted ON api_keys(deleted_at) WHERE deleted_at IS NULL;

-- Add comment to explain the column
COMMENT ON COLUMN api_keys.deleted_at IS 'Timestamp when the API key was soft-deleted. NULL means the key is not deleted.';
COMMENT ON COLUMN api_keys.is_active IS 'Whether the API key is active (enabled) or paused. Deleted keys (deleted_at IS NOT NULL) should be filtered out regardless of this flag.';

