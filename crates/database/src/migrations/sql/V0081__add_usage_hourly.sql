-- Derived hourly aggregate of organization_usage_log for analytics reads.
-- Rebuilt from raw by UsageHourlyScheduler (replace semantics); never written by the usage path.
-- No foreign keys: the table is derived and can be regenerated from raw at any time.
CREATE TABLE IF NOT EXISTS usage_hourly (
    hour                   TIMESTAMPTZ NOT NULL,
    organization_id        UUID NOT NULL,
    workspace_id           UUID NOT NULL,
    api_key_id             UUID NOT NULL,
    model_id               UUID NOT NULL,
    model_name             TEXT NOT NULL,
    inference_type         TEXT,
    served_provider_type   TEXT,
    served_provider_tier   TEXT,
    served_via_fallback    BOOLEAN NOT NULL,
    request_count          BIGINT NOT NULL,
    input_tokens           BIGINT NOT NULL,
    output_tokens          BIGINT NOT NULL,
    cache_read_tokens      BIGINT NOT NULL,
    total_tokens           BIGINT NOT NULL,
    total_cost             BIGINT NOT NULL,
    error_count            BIGINT NOT NULL,
    incomplete_count       BIGINT NOT NULL,
    stop_reason_count      BIGINT NOT NULL,
    ttft_count             BIGINT NOT NULL,
    ttft_sum_ms            BIGINT NOT NULL,
    ttft_p50_ms            DOUBLE PRECISION,
    ttft_p95_ms            DOUBLE PRECISION,
    ttft_p99_ms            DOUBLE PRECISION,
    itl_count              BIGINT NOT NULL,
    itl_sum_ms             DOUBLE PRECISION NOT NULL,
    itl_p95_ms             DOUBLE PRECISION,
    last_usage_at          TIMESTAMPTZ NOT NULL,
    CONSTRAINT usage_hourly_grain UNIQUE NULLS NOT DISTINCT
        (hour, organization_id, workspace_id, api_key_id, model_id, model_name,
         inference_type, served_provider_type, served_provider_tier, served_via_fallback)
);

CREATE INDEX IF NOT EXISTS idx_usage_hourly_org_hour ON usage_hourly (organization_id, hour);
CREATE INDEX IF NOT EXISTS idx_usage_hourly_key ON usage_hourly (workspace_id, api_key_id);

COMMENT ON TABLE usage_hourly IS
    'Hourly UTC aggregate of organization_usage_log (inference only), recomputed from raw; nano-USD costs';
