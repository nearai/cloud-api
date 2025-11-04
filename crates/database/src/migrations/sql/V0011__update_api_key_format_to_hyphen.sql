-- Update API key prefix format to use hyphen (sk-) instead of underscore (sk_)
UPDATE api_keys SET key_prefix = 'sk-****' WHERE key_prefix = 'sk_****';

-- Update comment to reflect the standard format with hyphen
COMMENT ON COLUMN api_keys.key_prefix IS 'First 8-10 characters of the API key for display purposes (e.g., "sk-abc123")';

