-- Covering index for the /v1/users/me workspaces JOIN query.
-- The query joins workspaces → organization_members on organization_id,
-- then filters is_active = true. A composite index on (organization_id, is_active)
-- lets Postgres satisfy both the join and the filter with a single index scan.
CREATE INDEX IF NOT EXISTS idx_workspaces_org_active
    ON workspaces (organization_id, is_active);
