-- V19: Add changed_by_user_email to organization_limits_history
-- This migration adds the email of the admin user who made the change for easier auditing

-- Add changed_by_user_email column to organization_limits_history
ALTER TABLE organization_limits_history
ADD COLUMN changed_by_user_email VARCHAR(255);

-- Create index for efficient queries by email
CREATE INDEX idx_org_limits_history_changed_by_email ON organization_limits_history(changed_by_user_email);

-- Add comment to explain the column
COMMENT ON COLUMN organization_limits_history.changed_by_user_email IS 'The email of the authenticated admin user who made the change (for easier auditing without requiring a user lookup).';

