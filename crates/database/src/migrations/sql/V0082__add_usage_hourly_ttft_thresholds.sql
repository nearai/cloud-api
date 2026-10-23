-- Per-hour counts of requests whose TTFT is strictly below 5s, 10s and 60s, for SLA shares
-- on the organization metrics report (denominator: ttft_count).
-- Hours already aggregated lack these counts, so empty the derived table: UsageHourlyScheduler
-- rebuilds it from raw, and readers serve raw rows for uncomputed hours meanwhile.
-- No DEFAULT: a pod still on the previous build inserts without these columns and must fail
-- (its recompute transaction rolls back) rather than store zeros.
TRUNCATE usage_hourly;

ALTER TABLE usage_hourly
    ADD COLUMN ttft_under_5s_count  BIGINT NOT NULL,
    ADD COLUMN ttft_under_10s_count BIGINT NOT NULL,
    ADD COLUMN ttft_under_60s_count BIGINT NOT NULL;
