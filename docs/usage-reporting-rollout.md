# Programmatic usage reporting rollout

Programmatic usage reporting is disabled unless `USAGE_REPORTING_ENABLED=true`. Keep it disabled until all six concurrent indexes below are valid and ready on the database used by each Cloud API deployment. When the flag is enabled, Cloud API repeats this check at startup and refuses to start if a prerequisite is missing.

Reporting-token management and the OpenAPI description remain available while reporting is disabled. Export and summary routes return 404 until the flag is enabled, so tokens can be provisioned before a staged rollout without exposing the query workload.

## 1. Build the indexes

Run the out-of-band script outside a transaction during a low-traffic window:

```bash
psql "$DATABASE_URL" \
  --set ON_ERROR_STOP=1 \
  --file crates/database/src/migrations/out_of_band/usage_reporting_indexes.sql
```

`CREATE INDEX CONCURRENTLY` can leave an invalid index after interruption. Re-run the script only after checking and removing any invalid index with the same name.

## 2. Verify the prerequisite

This query must return six rows with both booleans set to `t`:

```sql
WITH required(index_name, table_name) AS (
    VALUES
        ('idx_org_usage_reporting_org_created_id', 'organization_usage_log'),
        ('idx_org_usage_reporting_org_workspace_created_id', 'organization_usage_log'),
        ('idx_org_usage_reporting_org_api_key_created_id', 'organization_usage_log'),
        ('idx_org_service_usage_reporting_org_created_id', 'organization_service_usage_log'),
        ('idx_org_service_usage_reporting_org_workspace_created_id', 'organization_service_usage_log'),
        ('idx_org_service_usage_reporting_org_api_key_created_id', 'organization_service_usage_log')
)
SELECT required.index_name,
       COALESCE(pg_index.indisvalid, false) AS is_valid,
       COALESCE(pg_index.indisready, false) AS is_ready
FROM required
LEFT JOIN pg_namespace
  ON pg_namespace.nspname = current_schema()
LEFT JOIN pg_class AS table_class
  ON table_class.relnamespace = pg_namespace.oid
 AND table_class.relname = required.table_name
 AND table_class.relkind IN ('r', 'p')
LEFT JOIN pg_class AS index_class
  ON index_class.relnamespace = pg_namespace.oid
 AND index_class.relname = required.index_name
 AND index_class.relkind = 'i'
LEFT JOIN pg_index
  ON pg_index.indexrelid = index_class.oid
 AND pg_index.indrelid = table_class.oid
ORDER BY required.index_name;
```

Do not enable reporting if a row is missing or either value is `f`.

## 3. Check representative query plans

Run `EXPLAIN (ANALYZE, BUFFERS)` with a high-volume organization and a 366-day window for the default, workspace, API-key, and model-filtered export shapes. Replace the values below with non-sensitive production identifiers:

```sql
EXPLAIN (ANALYZE, BUFFERS)
SELECT id, created_at
FROM organization_usage_log
WHERE organization_id = '<organization-uuid>'
  AND created_at >= NOW() - INTERVAL '366 days'
  AND created_at <= NOW()
ORDER BY created_at DESC, id DESC
LIMIT 1001;

-- Repeat with each additional predicate separately:
-- AND workspace_id = '<workspace-uuid>'
-- AND api_key_id = '<api-key-uuid>'
-- AND model_name = '<model-name>'
```

Repeat the default, workspace, and API-key shapes for `organization_service_usage_log`. The default/workspace/API-key plans should use the corresponding reporting index. Model filtering should still use the organization/time index before applying the model predicate. Do not enable the feature if a high-volume organization unexpectedly uses an unbounded sequential scan or if representative execution approaches the database deadline (12 seconds with the default 15-second request timeout).

## 4. Enable with bounded limits

Set the feature flag and restart Cloud API instances gradually:

```dotenv
USAGE_REPORTING_ENABLED=true
USAGE_REPORTING_GLOBAL_REQUESTS_PER_MINUTE=600
USAGE_REPORTING_TOKEN_REQUESTS_PER_MINUTE=60
USAGE_REPORTING_MAX_CONCURRENT_REQUESTS=4
USAGE_REPORTING_TOKEN_MAX_CONCURRENT_REQUESTS=2
USAGE_REPORTING_REQUEST_TIMEOUT_SECONDS=15
```

The values shown are the application defaults. PostgreSQL statements are cancelled at 80% of the request deadline, leaving time to return a structured HTTP 504. Limits are enforced per Cloud API process; ingress-level client/IP throttling remains required for fleet-wide protection.

## 5. Smoke test

An invalid reporting token must be rejected before a reporting query runs:

```bash
curl --fail-with-body --silent --show-error \
  --header 'Authorization: Bearer rpt-00000000000000000000000000000000' \
  "$CLOUD_API_URL/v1/organizations/$ORGANIZATION_ID/usage/summary"
```

Expect HTTP 401. Then use a short-lived token created for the test organization and verify export and summary return HTTP 200:

```bash
curl --fail-with-body --silent --show-error \
  --header "Authorization: Bearer $REPORTING_TOKEN" \
  "$CLOUD_API_URL/v1/organizations/$ORGANIZATION_ID/usage/export?limit=1"

curl --fail-with-body --silent --show-error \
  --header "Authorization: Bearer $REPORTING_TOKEN" \
  "$CLOUD_API_URL/v1/organizations/$ORGANIZATION_ID/usage/summary"
```

Revoke the smoke-test token immediately afterward.

## 6. Monitor and roll back

Watch database pool saturation, PostgreSQL query latency/timeouts, lock waits, API 429/504 rates, CPU, and replica lag. If reporting causes pressure, set `USAGE_REPORTING_ENABLED=false` and restart the affected instances. Disabling the routes does not remove reporting tokens or indexes, so they can be re-enabled after the cause is addressed.
