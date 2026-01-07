-- no-transaction
-- V39: Fix workspace name unique constraint to only apply to active workspaces
-- This allows reusing workspace names after a workspace has been soft-deleted (is_active = false)
--
-- NOTE: This migration runs without a transaction because CREATE INDEX CONCURRENTLY
-- cannot be executed inside a transaction block. CONCURRENTLY is used to avoid
-- locking the table during index creation on large tables.

-- Drop the existing unique constraint on (organization_id, name)
-- Using IF EXISTS to make migration idempotent and safer for production
ALTER TABLE workspaces
DROP CONSTRAINT IF EXISTS workspaces_organization_id_name_key;

-- Create a partial unique index that only applies to active workspaces
-- This allows multiple inactive workspaces to have the same name,
-- but ensures active workspaces have unique name values within an organization
CREATE UNIQUE INDEX CONCURRENTLY unique_workspace_name_per_org_active_only
ON workspaces(organization_id, name)
WHERE is_active = true;
