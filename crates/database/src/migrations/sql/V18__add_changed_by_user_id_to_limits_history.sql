-- V18: Add changed_by_user_id to organization_limits_history
-- This migration adds a foreign key to track which admin user made the change

-- Add changed_by_user_id column to organization_limits_history
ALTER TABLE organization_limits_history
ADD COLUMN changed_by_user_id UUID REFERENCES users(id);

-- Create index for efficient queries
CREATE INDEX idx_org_limits_history_changed_by_user ON organization_limits_history(changed_by_user_id);

-- Add comment to explain the column
COMMENT ON COLUMN organization_limits_history.changed_by_user_id IS 'The authenticated user ID who made the change (for audit purposes). This is the authoritative user ID, unlike changed_by which is a self-reported string.';

