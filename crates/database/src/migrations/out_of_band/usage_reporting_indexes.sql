-- Usage reporting production indexes.
--
-- Run this script manually with psql outside refinery and outside an explicit
-- transaction block before enabling high-volume reporting queries in production.
-- These indexes target append-heavy usage tables, so they use CONCURRENTLY to
-- avoid blocking writes during index builds.

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_org_usage_reporting_org_created_id
    ON organization_usage_log (organization_id, created_at DESC, id DESC);

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_org_usage_reporting_org_workspace_created_id
    ON organization_usage_log (organization_id, workspace_id, created_at DESC, id DESC);

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_org_usage_reporting_org_api_key_created_id
    ON organization_usage_log (organization_id, api_key_id, created_at DESC, id DESC);

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_org_service_usage_reporting_org_created_id
    ON organization_service_usage_log (organization_id, created_at DESC, id DESC);

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_org_service_usage_reporting_org_workspace_created_id
    ON organization_service_usage_log (organization_id, workspace_id, created_at DESC, id DESC);

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_org_service_usage_reporting_org_api_key_created_id
    ON organization_service_usage_log (organization_id, api_key_id, created_at DESC, id DESC);
