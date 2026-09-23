# Redis admission cache

This release keeps usage posting synchronous in PostgreSQL. Redis stores committed admission snapshots only. It is not a queue or a hard spending reservation system.

## Deployment prerequisites

Build on current main, which includes #1116, #1119, #1120, and the subsequent counter rollout fixes. Complete counter reconciliation before production cache activation for bounded primary reads. The key snapshot reader preserves main's raw inference-history fallback for unreconciled organizations, within the admission read deadline. Apply V0082 before new cache-capable instances serve requests. The accepted soft-cache rollout does not permit pre-counter writers.

Use a primary Redis/Valkey endpoint with cluster mode disabled, private application access, and TLS/auth configured by a secret `rediss://` URL. The client uses system trust roots and a reconnecting multiplexed connection. Redis loss affects cache freshness/performance; PostgreSQL remains billing authority. Redis nodes must have synchronized clocks because fills carry an absolute expiration measured in Redis server time.

The user will provision staging and production Redis after the Rust implementation. Runtime verification has not been performed as part of implementation.

## Configuration

| Environment variable | Default | Purpose |
|---|---|---|
| `ADMISSION_CACHE_ENABLED` | `false` | Enable cache reads and post-commit refresh at process startup |
| `ADMISSION_CACHE_REDIS_URL` | unset | Secret endpoint with optional credentials; never log it |
| `ADMISSION_CACHE_TTL_SECONDS` | `300` | Maximum snapshot lifetime |
| `ADMISSION_CACHE_TTL_JITTER_SECONDS` | `30` | Subtracted jitter: default lifetime 270–300 seconds |
| `ADMISSION_CACHE_COMMAND_DEADLINE_MS` | `100` | Redis operation budget, including lazy connection setup |
| `ADMISSION_CACHE_FALLBACK_CONCURRENCY` | `64` | Shared per-process database read/refresh slots |
| `ADMISSION_CACHE_FALLBACK_DEADLINE_MS` | `500` | Total permit-wait/read budget and refresh budget |
| `ADMISSION_CACHE_MAX_FILL_AGE_MS` | `500` | Maximum age accepted by the Redis write script |

These initial budgets require staging latency/load verification. Cache reads do not renew expiry. Accepted fresh refreshes can renew it; a lower revision cannot overwrite a present higher revision. Delayed fills are rejected at Redis script execution. Loss/eviction can discard revision history, so short stale fills remain possible inside the accepted fill-age window. Five minutes is not a dollar overspend bound.

A cache miss/error uses the same bounded primary reader as disabled mode. A failed primary read fails admission. A failed refresh is measured but never converts committed usage into a returned billing error. Cancellation after commit can miss refresh; expiration repairs that stale snapshot. Counter-based listing and billing summary readers remain unchanged.

Metrics: `admission_cache_operations` with fixed `outcome` labels, `admission_check_latency` with organization/key subject, and `admission_primary_latency`. Watch primary load as well as cache hit rate: committed usage refreshes still read PostgreSQL. Existing authentication, activation, staking preflight, and rate limits remain in their original owners.

## Verification after provisioning

Use a dedicated test PostgreSQL database and an isolated Redis endpoint. The Redis test uses random keys and does not flush the instance.

```bash
cargo fmt --all -- --check
cargo check -p api -p database -p services --all-targets --all-features
cargo nextest run -p services -E 'test(usage::admission::coordinator_tests::)'
cargo nextest run -p config -E 'test(admission_cache::tests::)'
cargo nextest run -p database --test admission_snapshot --test counter_readers --test spend_counter_backfill
# Set REDIS_ADMISSION_TEST_URL through the test environment, not shell history.
cargo nextest run -p services --run-ignored ignored-only -E 'test(usage::admission::redis::tests::)'
```

Also run the existing API-key, credit allocation, service usage, and staking regressions. In staging check top-ups/limit changes, Redis disconnect/restart, stale refresh rejection, fallback saturation, and primary failures. Compare admission/recording latency and PostgreSQL load before enabling production. Also exercise mixed-version writes: populate a snapshot through the new API, commit usage and limit changes through the previous counter-capable API, and verify a subsequent cache miss after the original TTL loads the changed primary state. Check both organization and key snapshots; cache hits must not extend expiry. Runtime results remain pending.

## Accepted rollout (2026-09-23)

1. Provision staging Redis and configure its secret URL plus `ADMISSION_CACHE_ENABLED=true` in the CVM deployment environment. Deploy the new API with caching enabled from the first deployment and complete the verification above.
2. Provision production Redis after staging validation. Use the same single rolling deployment with caching enabled, fleet-wide without organization cohorts. A disabled-first deployment is not required.
3. Monitor cache errors, admission/recording latency, and primary load during the rollout and after old instances and their in-flight writes have drained.

During mixed-version operation, old counter-capable writers still commit authoritative usage and limit changes but neither advance cache revisions nor refresh Redis. Admission may use a snapshot that omits those changes until its original expiry, at most 300 seconds with the default TTL. This soft-limit staleness is accepted. The rollout itself may last longer, and TTL is not a monetary overspend bound. Revisions do not validate cache hits against PostgreSQL; a fresh primary read includes committed changes even when old writers leave the revision unchanged.

The single enablement flag controls both cache reads and post-commit refresh. When disabled, admission uses PostgreSQL and refresh is a no-op, even if a Redis URL is supplied. Environment changes require CVM redeployment; there is no live toggle. To bypass Redis, redeploy the same image with `ADMISSION_CACHE_ENABLED=false`. Keep counter-capable writers and readiness prerequisites intact during rollback.
