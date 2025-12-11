-- V26: Add analytics indexes and latency metrics for enterprise dashboard queries
-- These composite indexes optimize time-range queries for analytics endpoints

-- Speed up workspace analytics queries (workspace breakdown by time)
CREATE INDEX IF NOT EXISTS idx_org_usage_workspace_time
    ON organization_usage_log(workspace_id, created_at DESC);

-- Speed up API key analytics queries (per-key usage breakdown)
CREATE INDEX IF NOT EXISTS idx_org_usage_key_time
    ON organization_usage_log(api_key_id, created_at DESC);

-- Speed up model analytics queries (model performance over time)
CREATE INDEX IF NOT EXISTS idx_org_usage_model_time
    ON organization_usage_log(model_id, created_at DESC);

-- Add latency columns (nullable for backward compatibility with existing rows)
-- TTFT (time to first token) and average ITL (inter-token latency) per request
ALTER TABLE organization_usage_log
    ADD COLUMN IF NOT EXISTS ttft_ms INTEGER,
    ADD COLUMN IF NOT EXISTS avg_itl_ms DOUBLE PRECISION;

-- Add index for latency analytics queries
CREATE INDEX IF NOT EXISTS idx_usage_log_model_latency
    ON organization_usage_log(model_id, ttft_ms, avg_itl_ms)
    WHERE ttft_ms IS NOT NULL;

-- Comments for documentation
COMMENT ON INDEX idx_org_usage_workspace_time IS 'Supports analytics queries filtering by workspace + time range';
COMMENT ON INDEX idx_org_usage_key_time IS 'Supports analytics queries filtering by API key + time range';
COMMENT ON INDEX idx_org_usage_model_time IS 'Supports analytics queries filtering by model + time range';
COMMENT ON COLUMN organization_usage_log.ttft_ms IS 'Time to first token in milliseconds (streaming only)';
COMMENT ON COLUMN organization_usage_log.avg_itl_ms IS 'Average inter-token latency in milliseconds (streaming only)';
