CREATE INDEX IF NOT EXISTS idx_org_service_usage_workspace_api_key
    ON organization_service_usage_log(workspace_id, api_key_id);
