-- Add targeted indexes for high-volume admin metrics queries.
--
-- These do not cache aggregate results and do not create new data tables. They
-- help Postgres narrow the large usage tables by time, org, model, and service
-- before the existing analytics queries aggregate.
--
-- refinery runs PostgreSQL migrations in a transaction, so CREATE INDEX
-- CONCURRENTLY is not available here. Keep lock acquisition bounded.

SET LOCAL lock_timeout = '5s';

CREATE INDEX IF NOT EXISTS idx_org_usage_created_org_model
    ON organization_usage_log(created_at, organization_id, model_name);

CREATE INDEX IF NOT EXISTS idx_org_usage_created_model_name
    ON organization_usage_log(created_at, model_name);

CREATE INDEX IF NOT EXISTS idx_org_usage_created_org
    ON organization_usage_log(created_at, organization_id);

CREATE INDEX IF NOT EXISTS idx_org_usage_created_total_cost
    ON organization_usage_log(created_at, total_cost);

CREATE INDEX IF NOT EXISTS idx_org_usage_org_created_model_name
    ON organization_usage_log(organization_id, created_at, model_name);

CREATE INDEX IF NOT EXISTS idx_org_usage_org_created_workspace
    ON organization_usage_log(organization_id, created_at, workspace_id);

CREATE INDEX IF NOT EXISTS idx_org_usage_org_created_api_key
    ON organization_usage_log(organization_id, created_at, api_key_id);

CREATE INDEX IF NOT EXISTS idx_org_service_usage_created_org
    ON organization_service_usage_log(created_at, organization_id);

COMMENT ON INDEX idx_org_usage_created_org_model IS
    'Supports admin platform/org analytics filtered by time and grouped by organization/model.';
COMMENT ON INDEX idx_org_usage_created_model_name IS
    'Supports admin model revenue and top-model analytics by time range.';
COMMENT ON INDEX idx_org_usage_created_org IS
    'Supports admin organization revenue and top-organization analytics by time range.';
COMMENT ON INDEX idx_org_usage_org_created_model_name IS
    'Supports per-organization model breakdowns by time range.';
COMMENT ON INDEX idx_org_usage_org_created_workspace IS
    'Supports per-organization workspace breakdowns by time range.';
COMMENT ON INDEX idx_org_usage_org_created_api_key IS
    'Supports per-organization API key breakdowns by time range.';
COMMENT ON INDEX idx_org_service_usage_created_org IS
    'Supports billing-summary service usage aggregation by time/organization dimensions.';
