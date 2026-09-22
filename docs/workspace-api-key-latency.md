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

The existing request-correlation middleware validates/generates a request UUID
and scopes it to downstream work using a task-local context. Every timing event
records `request_id` and `workspace_id` directly; handler events also record
`limit` and `offset`. Correlation therefore survives the production JSON
formatter's disabled current/ancestor-span fields. The formatter's privacy
settings remain unchanged. Spawned background tasks do not inherit the context.

Only the two `workspace_api_key_list_phase_finished` service summaries are INFO
by default (one if counting fails). More detailed completion timings and the
handler-entry event are DEBUG, under the dedicated `workspace_api_key_timing`
target. Enable just these safe timing events for an investigation with:

```sh
LOG_LEVEL=info,workspace_api_key_timing=debug
```

Set that target to `warn` to suppress timing events entirely. Detailed phase
start events were removed to avoid duplicating completion events.

| Event | Level | Meaning |
| --- | --- | --- |
| `workspace_api_key_list_started` | DEBUG | Handler entry after extraction and pagination validation |
| `workspace_api_key_permission_finished` | DEBUG | Permission-check duration for count and list separately, including their own database waits |
| `workspace_api_key_db_phase_finished` | DEBUG | Pool acquisition and SQL execution separately for count and usage-bearing list |
| `workspace_api_key_list_phase_finished` | INFO | Count/list service wall time, including permissions, retries, and mapping; list completion includes combined service wall time |

Finished events include `elapsed_ms` and `success`. A shared timing helper keeps
permission/pool/query fields consistent. The `operation` field uses
`count_api_keys` or `list_api_keys` across handler, service, and repository layers.
Repository completion events repeat on database retry. Service/permission/repository times are nested and
must not be added together. Combined service time excludes authentication
middleware, response serialization, gateway time, and network transit. Missing
completion events are incomplete evidence, not proof that PostgreSQL cancelled
work. Metadata failures log stable workspace/repository error categories and
request IDs rather than free-form error strings, which may contain customer
values.

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
TEST_DATABASE_NAME=cloud_api_key_lookup_test cargo nextest run -p api --test e2e_all \
  -E 'test(/^api_key_metadata::/)'

TEST_DATABASE_NAME=cloud_api_key_lookup_bench KEY_LOOKUP_USAGE_ROWS=100000 \
  cargo nextest run -p api --test e2e_all --run-ignored only \
  -E 'test(/^api_key_metadata::measure_workspace_key_lookup_latency$/)' \
  --success-output immediate
```

The lock regression holds ACCESS EXCLUSIVE locks on both usage tables and
requires the metadata request to complete within five seconds. This detects an
accidental dependency on usage aggregation without asserting production speed.
The nextest exclusive override prevents this lock test from overlapping any
other test. The opt-in benchmark uses the same exclusive override for isolated
measurements and global ANALYZE operations. Use nextest so these rules apply.

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

Collected at commit `30fde5b2`, before the review changes moved detailed timing
events from INFO to DEBUG and added explicit event-level request IDs.

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

The INFO events at that revision separated the 105 list requests in the large-history
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
