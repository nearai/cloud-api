-- V40: Fix organization name unique constraint to only apply to active organizations
-- This allows reusing organization names after an organization has been soft-deleted (is_active = false)

-- Drop the existing unique constraint on name
-- Using IF EXISTS to make migration idempotent and safer for production
ALTER TABLE organizations
DROP CONSTRAINT IF EXISTS organizations_name_key;

-- Create a partial unique index that only applies to active organizations
-- This allows multiple inactive organizations to have the same name,
-- but ensures active organizations have unique name values
CREATE UNIQUE INDEX unique_organization_name_active_only
ON organizations(name)
WHERE is_active = true;
