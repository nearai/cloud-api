-- V39: Fix workspace name unique constraint to only apply to active workspaces
-- This allows reusing workspace names after a workspace has been soft-deleted (is_active = false)

-- Drop the existing unique constraint on (organization_id, name)
ALTER TABLE workspaces
DROP CONSTRAINT workspaces_organization_id_name_key;

-- Create a partial unique index that only applies to active workspaces
-- This allows multiple inactive workspaces to have the same name,
-- but ensures active workspaces have unique name values within an organization
CREATE UNIQUE INDEX unique_workspace_name_per_org_active_only
ON workspaces(organization_id, name)
WHERE is_active = true;

