-- V5: Change user_id to api_key_id in organization_usage_log
-- This migration updates the usage tracking to use api_key_id instead of user_id
-- for better audit trail and tracking which API key was used

-- Add api_key_id column first (nullable initially)
ALTER TABLE organization_usage_log 
    ADD COLUMN IF NOT EXISTS api_key_id UUID REFERENCES api_keys(id) ON DELETE CASCADE;

-- For existing rows, we can't determine which API key was used, so we'll delete them
-- Since this is early in development and there's no production data yet
DELETE FROM organization_usage_log WHERE api_key_id IS NULL;

-- Now make the column NOT NULL
ALTER TABLE organization_usage_log 
    ALTER COLUMN api_key_id SET NOT NULL;

-- Drop the old user_id column
ALTER TABLE organization_usage_log 
    DROP COLUMN IF EXISTS user_id;

-- Drop old index and create new one
DROP INDEX IF EXISTS idx_org_usage_user;
CREATE INDEX IF NOT EXISTS idx_org_usage_api_key ON organization_usage_log(api_key_id);

-- Add comment
COMMENT ON COLUMN organization_usage_log.api_key_id IS 'API key used for this request (for audit trail)';

