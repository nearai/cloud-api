-- Add key_prefix to API keys table for displaying partial keys in UI
ALTER TABLE api_keys ADD COLUMN key_prefix VARCHAR(16);

-- Populate existing keys with a placeholder prefix
-- In practice, existing keys won't show a real prefix since we don't have the original key
UPDATE api_keys SET key_prefix = 'sk-****' WHERE key_prefix IS NULL;

-- Make key_prefix NOT NULL now that all rows have a value
ALTER TABLE api_keys ALTER COLUMN key_prefix SET NOT NULL;

-- Add index for quick lookups by prefix (useful for UI search)
CREATE INDEX idx_api_keys_prefix ON api_keys(key_prefix);

-- Add comment explaining the key_prefix field
COMMENT ON COLUMN api_keys.key_prefix IS 'First 8-10 characters of the API key for display purposes (e.g., "sk-abc123")';

