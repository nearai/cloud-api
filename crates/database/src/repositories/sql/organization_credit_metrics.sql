-- Each section reads the same materialized metric_usage, including live settlement
-- and reversal amounts. Convert nano-dollars only after integer aggregation.
, summary AS (
    SELECT
        COUNT(*)::bigint as total_requests,
        COALESCE(SUM(input_tokens), 0)::bigint as total_input_tokens,
        COALESCE(SUM(output_tokens), 0)::bigint as total_output_tokens,
        COALESCE(SUM(cache_read_tokens), 0)::bigint as total_cache_read_tokens,
        COALESCE(SUM(filtered_cost), 0)::bigint::double precision / 1000000000.0 as total_cost_usd,
        COUNT(DISTINCT api_key_id)::bigint as unique_api_keys,
        COUNT(ttft_ms)::bigint as ttft_measured_requests,
        COUNT(*) FILTER (WHERE ttft_ms < 5000)::bigint as ttft_under_5s_requests,
        COUNT(*) FILTER (WHERE ttft_ms < 10000)::bigint as ttft_under_10s_requests,
        COUNT(*) FILTER (WHERE ttft_ms < 60000)::bigint as ttft_under_60s_requests
    FROM metric_usage
)
, by_workspace AS (
    SELECT
        w.id as workspace_id,
        w.name as workspace_name,
        COUNT(ul.id)::bigint as requests,
        COALESCE(SUM(ul.input_tokens), 0)::bigint as input_tokens,
        COALESCE(SUM(ul.output_tokens), 0)::bigint as output_tokens,
        COALESCE(SUM(ul.cache_read_tokens), 0)::bigint as cache_read_tokens,
        COALESCE(SUM(ul.filtered_cost), 0)::bigint::double precision / 1000000000.0 as cost_usd
    FROM workspaces w
    LEFT JOIN metric_usage ul ON ul.workspace_id = w.id
    WHERE w.organization_id = $1
    GROUP BY w.id, w.name
)
, by_api_key AS (
    SELECT
        ak.id as api_key_id,
        ak.name as api_key_name,
        COUNT(ul.id)::bigint as requests,
        COALESCE(SUM(ul.filtered_cost), 0)::bigint::double precision / 1000000000.0 as cost_usd
    FROM api_keys ak
    LEFT JOIN metric_usage ul ON ul.api_key_id = ak.id
    WHERE ak.workspace_id IN (
        SELECT id FROM workspaces WHERE organization_id = $1
    )
    GROUP BY ak.id, ak.name
)
, by_model AS (
    SELECT
        ul.model_name,
        COUNT(*)::bigint as requests,
        COALESCE(SUM(ul.input_tokens), 0)::bigint as input_tokens,
        COALESCE(SUM(ul.output_tokens), 0)::bigint as output_tokens,
        COALESCE(SUM(ul.cache_read_tokens), 0)::bigint as cache_read_tokens,
        COALESCE(SUM(ul.filtered_cost), 0)::bigint::double precision / 1000000000.0 as cost_usd,
        AVG(ul.ttft_ms)::double precision as avg_ttft_ms,
        PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY ul.ttft_ms)::double precision as p95_ttft_ms,
        AVG(ul.avg_itl_ms)::double precision as avg_itl_ms,
        PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY ul.avg_itl_ms)::double precision as p95_itl_ms,
        COUNT(ul.ttft_ms)::bigint as ttft_measured_requests,
        COUNT(*) FILTER (WHERE ul.ttft_ms < 5000)::bigint as ttft_under_5s_requests,
        COUNT(*) FILTER (WHERE ul.ttft_ms < 10000)::bigint as ttft_under_10s_requests,
        COUNT(*) FILTER (WHERE ul.ttft_ms < 60000)::bigint as ttft_under_60s_requests
    FROM metric_usage ul
    GROUP BY ul.model_name
)
SELECT
    (SELECT to_jsonb(summary) FROM summary) AS summary,
    COALESCE(
        (SELECT jsonb_agg(by_workspace ORDER BY requests DESC) FROM by_workspace),
        '[]'::jsonb
    ) AS by_workspace,
    COALESCE(
        (SELECT jsonb_agg(by_api_key ORDER BY requests DESC) FROM by_api_key),
        '[]'::jsonb
    ) AS by_api_key,
    COALESCE(
        (SELECT jsonb_agg(by_model ORDER BY requests DESC) FROM by_model),
        '[]'::jsonb
    ) AS by_model
