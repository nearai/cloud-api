-- Add OAuth states table for cross-instance OAuth state sharing
-- This migration adds a table to store temporary OAuth state parameters
-- to support multi-instance deployments where OAuth callbacks may hit
-- different instances than where the OAuth flow was initiated.

CREATE TABLE oauth_states (
    -- OAuth state parameter (UUID format, used as CSRF token)
    state VARCHAR(64) PRIMARY KEY,

    -- OAuth provider: "github" or "google"
    provider VARCHAR(20) NOT NULL,

    -- PKCE verifier for Google OAuth (NULL for GitHub which doesn't use PKCE)
    pkce_verifier TEXT,

    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL,

    -- Ensure provider is valid
    CONSTRAINT oauth_states_valid_provider CHECK (provider IN ('github', 'google'))
);

-- Index for efficient expiration checks in queries
-- Used in: WHERE expires_at > NOW() to filter out expired states
CREATE INDEX idx_oauth_states_expires ON oauth_states(expires_at);
