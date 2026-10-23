---
paths:
  - "crates/api/tests/**"
  - "crates/**/tests/**"
  - "crates/**/*test*.rs"
  - ".config/nextest.toml"
---

# Testing

Test commands are listed under Commands in `CLAUDE.md`.

## Tests must be able to fail

A test earns its place only if it fails when the behavior it covers breaks. Exercise the risky path itself (the retry, the race, the error branch, the copy that must be persisted), not a neighboring happy path. A race test must actually interleave the competing operations. Never weaken, skip, or delete an assertion to make a failing test pass; fix the code, or explain in the PR why the old assertion was wrong.

## E2E tests share one database

E2E tests are modules in `crates/api/tests/e2e_all/`, compiled into one binary, and run through `cargo nextest run --test e2e_all` so the bootstrap, concurrency limits, and serialized overrides in `.config/nextest.toml` apply. Helpers live in `crates/api/tests/common/mod.rs` (`setup_test_server`, `setup_unique_test_session`, `create_org`, `get_api_key_for_org`, `create_org_and_api_key`). Nextest processes share one real PostgreSQL database whose state persists between runs.

- Treat database state as mutable between requests and independent of test order.
- Bootstrapped shared fixtures are read-only. A test that creates or mutates users, organizations, workspaces, keys, models, aliases, jobs, schedules, or usage creates its own UUID-scoped resources and asserts only on test-owned IDs, names, or filters.
- Register test-owned model names with the mock provider pool when exercising inference.
- Never assert on global totals or ordering across requests. Prefer direct lookup or keyset pagination. With offset pagination over mutable rows, rescan until two consecutive snapshots agree; deduplication alone does not catch rows skipped after a concurrent delete.
- A test that changes database-wide state (DDL, global queues or schedulers, broad scans or updates) goes in a serialized override in `.config/nextest.toml`. Keep e2e concurrency and per-test pool limits within PostgreSQL capacity, counting secondary pools and background workers.
- Make tests reliable through isolation and deterministic synchronization. Wait for asynchronous outcomes with deadline-bounded polling of the exact observable result. Keep test requests and assertions single-attempt so races stay visible; bounded retries belong only in idempotent bootstrap before tests start.
- Concurrent nextest invocations use different, disposable `TEST_DATABASE_NAME` values.
