-- V15: Add admin access token table
-- This migration adds a table to track admin access tokens for proper revocation support
-- and security management

-- Admin access token table for tracking and managing admin access tokens
CREATE TABLE admin_access_token (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    token_hash VARCHAR(64) NOT NULL UNIQUE,
    name VARCHAR(255) NOT NULL, -- e.g. "Billing Service Token"
    created_by_user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    creation_reason TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL,
    last_used_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    revoked_by_user_id UUID REFERENCES users(id),
    revocation_reason TEXT NOT NULL,
    is_active BOOLEAN NOT NULL DEFAULT true,
);

-- Add indexes for efficient querying
CREATE INDEX idx_admin_access_token_hash ON admin_access_token(token_hash);
CREATE INDEX idx_admin_access_token_created_by ON admin_access_token(created_by_user_id);
CREATE INDEX idx_admin_access_token_active ON admin_access_token(is_active);
CREATE INDEX idx_admin_access_token_expires ON admin_access_token(expires_at);
CREATE INDEX idx_admin_access_token_last_used ON admin_access_token(last_used_at);
