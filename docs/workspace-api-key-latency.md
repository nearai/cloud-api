# Workspace API-key lookup latency

Related issue: https://github.com/nearai/cloud-api/issues/1105 (keep open).

## Scope and rollout

`GET /v1/workspaces/{workspace_id}/api-keys/{key_id}` returns metadata using
workspace membership checks and the existing primary-key lookup. It does not
aggregate either usage table. `key` is null and `usage` is omitted: absence of
usage is not a zero balance. An inactive key is returned with `is_active: false`;
deleted keys, mismatched workspace/key IDs, and inactive or missing parents
return 404. Unauthenticated requests return 401; nonmembers return 403.

Deploy and smoke-test the API endpoint on every serving instance and gateway
before deploying the Cloud UI adapter. The UI interprets 404 as an absent key
and does not fall back to listing on errors. Credential confirmation still
checks ownership, active state, and initial spending limit before saving the
key; its retry/rollback behavior is retained. Limit calculations still require
real usage. Recovery without a known key ID still needs the name-based list.

This change does not optimize the existing list query. The original incident's
root cause and production latency remain unverified.

## Runtime evidence

The existing request-correlation middleware attaches `request_id`, method, and
path to the HTTP tracing span. The list handler adds workspace ID, limit, and
offset without logging keys, authorization headers, or response bodies.
At INFO level it now emits:

| Event | Meaning |
| --- | --- |
| `workspace_api_key_list_started` | Request passed extraction and pagination validation and entered the handler work |
| `workspace_api_key_permission_started` / `finished` | Permission-check duration for count and list separately, including their own database waits |
| `workspace_api_key_db_phase_started` / `finished` | Pool acquisition and SQL execution separately for count and usage-bearing list |
| `workspace_api_key_list_phase_finished` | Count/list service wall time, including permission checks, database retries, and mapping; list completion also includes combined service wall time |

Finished events include `elapsed_ms` and `success`. Repository phase events
repeat on database retry; correlate them with existing retry attempt logs.
Service/permission/repository times are nested and must not be added together.
The combined service time excludes authentication middleware, response
serialization, gateway time, and network transit. A started phase without a
finished phase is incomplete evidence, not proof that PostgreSQL cancelled it.

For the reported 2026-09-21 18:49:00–18:50:24 UTC window:

1. Identify the deployed environment/revision and correlate the four GETs by
   workspace, time, and upstream request ID. Confirm whether they reached the
   handler; UI-generated 503s do not establish upstream status.
2. Split permission time, count pool/query time, and list pool/query time. Check
   database retry logs and gateway timings when those do not explain wall time.
3. Inspect actual `pg_indexes`, table cardinality for the workspace, statistics,
   and lock waits. In particular verify the service usage `(workspace_id,
   api_key_id)` index from V0067 and inference workspace indexes are deployed.
4. In a representative staging database, run the exact repository list SQL with
   `EXPLAIN (ANALYZE, BUFFERS, SETTINGS)` for `limit=100, offset=0`, default
   ordering, and separately usage ordering. Record database version, row counts,
   index definitions, buffer hits/reads, temporary spills, and the plan. Merely
   reducing key count or changing LIMIT does not bound historical aggregation.
5. Test a client disconnect at 20 seconds while observing backend/database
   activity. Determine whether work continues and overlaps subsequent retries;
   do not infer cancellation from an aborted HTTP fetch alone.

## Reproducible local checks

Use a dedicated disposable PostgreSQL database, never a production database.
The E2E harness creates/migrates `TEST_DATABASE_NAME`; supply normal test DB
connection variables (`DATABASE_HOST`, `DATABASE_PORT`, `DATABASE_USERNAME`,
`DATABASE_PASSWORD`). Examples below assume these are already configured.

```sh
TEST_DATABASE_NAME=cloud_api_key_lookup_test cargo test -p api --test e2e_all \
  api_key_metadata::metadata_lookup -- --test-threads=1 --nocapture

TEST_DATABASE_NAME=cloud_api_key_lookup_bench KEY_LOOKUP_USAGE_ROWS=100000 \
  RUST_LOG=info cargo test -p api --test e2e_all \
  api_key_metadata::measure_workspace_key_lookup_latency \
  -- --ignored --test-threads=1 --nocapture
```

The lock regression holds ACCESS EXCLUSIVE locks on both usage tables and
requires the metadata request to complete within two seconds. This detects an
accidental dependency on usage aggregation without asserting production speed.
Run these tests serially so locks do not interfere with unrelated usage tests.

The opt-in benchmark creates 64 keys and the configured number of inference
rows AND service rows, distributed across the keys. It analyzes those tables,
then measures metadata GET and the unchanged first list page at concurrency 1
and 4 (four database pool connections). One warm-up batch is discarded per
case; 20 batches produce 20 or 80 samples. `KEY_LOOKUP_BENCH` JSON records
workspace ID, row count, p50/p95/max milliseconds, and failures. Requests have a
20-second deadline; failures include non-200 responses and deadline failures.
This uses the in-process test transport and mock authentication; it excludes
real network/gateway latency and is a warm-cache synthetic workload. Rows are
retained in the dedicated test DB for plan inspection. Repeated runs add data;
use a fresh database when comparing dataset sizes.

## Local measurements (2026-09-22)

Environment: Apple Silicon macOS, PostgreSQL 17.9 (Homebrew), Rust debug test
build, INFO tracing, in-process Axum test transport, mock authentication, four
pool connections. Separate fresh databases were used for each history size.
All cases used 64 keys, `limit=100&offset=0` and default list ordering; one warm-up
batch preceded 20 measured batches. These are synthetic warm-cache results,
not a reproduction of the production incident or evidence for closing #1105.

| Rows per usage table | Concurrency | Endpoint | Samples | p50 ms | p95 ms | max ms | Failures |
| ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 0 | 1 | metadata | 20 | 1.64 | 2.12 | 2.75 | 0 |
| 0 | 1 | list | 20 | 6.05 | 7.16 | 7.26 | 0 |
| 0 | 4 | metadata | 80 | 2.33 | 2.88 | 3.93 | 0 |
| 0 | 4 | list | 80 | 9.59 | 20.46 | 29.32 | 0 |
| 100,000 | 1 | metadata | 20 | 1.53 | 1.94 | 2.03 | 0 |
| 100,000 | 1 | list | 20 | 25.62 | 26.35 | 28.43 | 0 |
| 100,000 | 4 | metadata | 80 | 2.07 | 2.44 | 2.76 | 0 |
| 100,000 | 4 | list | 80 | 28.78 | 31.07 | 31.55 | 0 |

The new INFO events also separated the 105 list requests in the large-history
run (including warm-up batches, both concurrency levels). Durations below are
integer milliseconds, so 0 means below one millisecond, not zero work.

| Phase | p50 ms | p95 ms | max ms |
| --- | ---: | ---: | ---: |
| Count permission check | 1 | 2 | 4 |
| Count pool acquisition | 0 | 0 | 0 |
| Count SQL | 0 | 0 | 0 |
| List permission check | 1 | 1 | 1 |
| List pool acquisition | 0 | 0 | 0 |
| List SQL including usage aggregation | 22 | 25 | 27 |

These logs point to aggregation as the dominant component in this local
fixture, with no observed pool pressure. They are not production evidence.

The exact list SQL on the 100,000-row-per-table fixture took 54.60 ms
in `EXPLAIN (ANALYZE, BUFFERS, SETTINGS)`. It scanned 100,000 rows from each
usage table via sequential scans and aggregated them into 64 key groups; the
API-key scan returned 64 rows. Usage-table blocks were buffer hits, with no
physical reads reported in this run. This supports the expected dependence
on usage history locally; it does not establish what caused the production
20-second deadlines. The empty-history four-request tail was noisy, so do not
extrapolate a scaling curve from these small samples.

The metadata regression also completed with both usage tables locked (about
19.5 ms including transaction rollback/assertion overhead). Unlike the benchmark,
that regression used DEBUG test logging. It proves the lookup does not depend
on reading usage tables; it is not a latency SLO measurement.

## Acceptance still required for #1105

Record measurements under representative workspace history, data skew,
concurrency, and cache conditions in the affected deployment or a faithful
staging environment. The original first-page list must reliably complete
within the existing 20-second client deadline; report sample size, tail latency,
errors, and timeouts, not only averages. Identify and fix the original
bottleneck, and verify that abandoned requests do not accumulate harmful work.
Neither passing the metadata regression nor a fast synthetic benchmark closes
this issue.
