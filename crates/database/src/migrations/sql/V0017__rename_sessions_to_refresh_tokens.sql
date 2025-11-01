-- Rename sessions table to refresh_tokens for clarity
-- This table stores refresh tokens for OAuth authentication

-- Rename the table
ALTER TABLE sessions RENAME TO refresh_tokens;

-- Rename the indexes
ALTER INDEX idx_sessions_hash RENAME TO idx_refresh_tokens_hash;
ALTER INDEX idx_sessions_user RENAME TO idx_refresh_tokens_user;
ALTER INDEX idx_sessions_expires RENAME TO idx_refresh_tokens_expires;

