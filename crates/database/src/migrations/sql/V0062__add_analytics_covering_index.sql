-- V0062: Add covering index for analytics timeseries queries.
--
-- The two new admin analytics endpoints (model-consumption-timeseries and
-- performance-timeseries) scan organization_usage_log filtered by created_at and
-- aggregate model_name, total_cost, total_tokens, ttft_ms, and stop_reason.
--
-- At production table sizes (50M+ rows / ~3-4 GB) the existing single-column
-- idx_org_usage_created forces a bitmap heap fetch for every matched row.  A
-- covering index converts range scans into index-only scans (no heap fetch),
-- which is substantially faster when the date range selectivity is < ~10%.
--
-- INCLUDE columns are read-only in the index; they add ~30 bytes per row but
-- impose zero write overhead beyond what the index key already has.  The write
-- path for organization_usage_log is append-only (INSERT only, no UPDATEs to
-- these columns), so index maintenance cost is a one-time insert cost.
--
-- Use CONCURRENTLY so the build does not lock the table in production.

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_org_usage_created_covering
    ON organization_usage_log (created_at DESC)
    INCLUDE (model_name, total_cost, total_tokens, output_tokens, ttft_ms, stop_reason);

COMMENT ON INDEX idx_org_usage_created_covering IS
    'Covering index for analytics timeseries endpoints; avoids heap fetch for model_name, cost, tokens, ttft_ms, stop_reason on date-range scans';

-- Separate index for the model-filtered performance-timeseries path
-- (AND ul.model_name = $3), which benefits from model_name as the lead column.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_org_usage_model_name_created
    ON organization_usage_log (model_name, created_at DESC)
    INCLUDE (total_tokens, output_tokens, ttft_ms, stop_reason);

COMMENT ON INDEX idx_org_usage_model_name_created IS
    'Supports model-filtered performance-timeseries queries (model_name = $3 AND created_at range)';
