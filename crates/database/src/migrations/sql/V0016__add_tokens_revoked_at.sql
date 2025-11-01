-- Add tokens_revoked_at column to users table
-- This is used to invalidate all access tokens issued before this timestamp
ALTER TABLE users ADD COLUMN tokens_revoked_at TIMESTAMPTZ;

-- Add index for efficient lookups
CREATE INDEX idx_users_tokens_revoked_at ON users(tokens_revoked_at) WHERE tokens_revoked_at IS NOT NULL;

