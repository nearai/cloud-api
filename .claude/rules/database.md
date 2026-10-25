---
paths:
  - "crates/database/**"
  - "crates/services/src/**/ports.rs"
  - "crates/services/src/usage/**"
  - "crates/services/src/admin/**"
  - "crates/services/src/admin_settings/**"
  - "crates/services/src/organization_credits/**"
  - "crates/services/src/reporting_tokens/**"
  - "crates/services/src/service_usage/**"
  - "crates/services/src/staking_farm/**"
  - "crates/services/src/models/**"
  - "crates/services/src/aml.rs"
  - "crates/services/src/email.rs"
  - "crates/services/src/common.rs"
  - "crates/services/src/reporting_usage.rs"
  - "crates/services/src/staking_farm.rs"
  - "crates/api/src/routes/**"
---

# Database and migrations

PostgreSQL 16 behind Patroni, pooled with deadpool. Repositories in `crates/database/src/repositories/` implement the service-layer traits from `ports.rs`.

## Database freeze (until phase 2)

The schema and the data logic for chat-completion usage, admin APIs, admin config and user console APIs moved to `nearai/cloud-platform`, which now owns the schema. Its migrations are byte-identical copies of ours at commit `ba1e37ba` (manifest-checked), and it has a drift check against this repo. Until phase 2 removes the moved code from here, a database change that lands only in cloud-api silently diverges the two services. No migration has landed here since the pin.

1. **No new or edited files in `crates/database/src/migrations/`** (`sql/` and `out_of_band/`). Schema changes go to cloud-platform (`crates/schema`); coordinate before cloud-api relies on them.
2. **No changes to SQL, queries, transactions, or row types** in the frozen repositories below, and no behavior changes to the moved services and routes, unless the same change is made in cloud-platform as a coordinated pair of PRs, each linking the other. Bug fixes follow the same rule: fix in both.
3. **Allowed**: the `cloud_api_responses` tables, code that calls frozen repositories without changing their SQL or behavior, docs, and tests that do not change behavior.
4. **Exceptions** are stated in the PR and approved under the repo's normal review requirements.
5. The freeze lifts when phase 2 cuts cloud-api over to cloud-platform and deletes the moved code.

Frozen repositories (`crates/database/src/repositories/`, tables owned by `cloud-platform`): `admin_access_token`, `admin_composite`, `admin_settings`, `aml`, `analytics` (and `analytics/`), `api_key`, `attestation`, `contact_email`, `credit_allocation`, `credit_reconciliation`, `credit_transfers`, `feature_request`, `lifecycle`, `model`, `model_alias`, `model_repository_impl`, `near_nonce`, `oauth_state`, `organization`, `organization_credentials`, `organization_invitation`, `organization_limits`, `organization_limits_repository_impl`, `organization_reporting_token`, `organization_reporting_token_row`, `organization_service_usage`, `organization_service_usage_reporting_summary`, `organization_staking_farm_sources`, `organization_usage`, `organization_usage_discount`, `organization_usage_reporting`, `organization_usage_reporting_summary`, `reporting_query`, `reporting_usage_summary`, `service`, `service_usage_repository_impl`, `session`, `staking_wallet_binding`, `usage_hourly`, `usage_repository_impl`, `user`, `user_erasure`, `workspace`, plus `sql/`, `retry.rs`, `utils.rs`, and `crates/database/src/{pool,models}.rs`. `user_erasure.rs` also deletes `cloud_api_responses` rows; those statements stay editable only if no `cloud-platform` table statement changes.

Frozen services and routes: `crates/services/src/{usage,admin,admin_settings,organization_credits,reporting_tokens,service_usage,staking_farm,models}`, `crates/services/src/{aml,email,common,reporting_usage,staking_farm}.rs`, and the route modules for usage, billing, admin, services, feature requests, organizations, organization members, workspaces, users, reporting, credit transfers, credentials, policy and staking farm in `crates/api/src/routes/`. These lists are the freeze; cloud-platform's drift check (`scripts/cloud-api-moved-paths.txt`) watches every path in them.

Not frozen (`cloud_api_responses` tables): `conversations`, `database_encryption_jobs`, `files`, `mcp_connector_usage`, `mcp_connectors`, `response_items`, `responses`, in `conversation`, `response`, `response_item`, `file`, and `mcp_connector`. Table ownership: `crates/schema/ownership.toml` in cloud-platform. A table not listed there is a schema change: rule 1.

## Prefer typed Rust parsing over a new migration

Before adding a migration, check whether existing columns can already encode the new state. If they can, add a typed layer in the repository that parses the row into an enum, and have services consume the enum, not raw columns. `UserLifecycle::from_columns` in `crates/services/src/lifecycle.rs` is the reference shape.

- Only operator-owned columns with a known value set may carry state: flags, provider names, status values, relationships between rows.
- Never encode state in customer-editable or free-form data (`organizations.settings`, names, descriptions). Customers can write those, so the state could be forged or erased.
- Parsing is strict: a combination of column values that maps to no variant is an error, never a silent default. Watch for `match s.as_str() { ..., _ => SomeVariant }`.
- Write sentinel values through their constant (for example `ERASED_AUTH_PROVIDER`), never a string literal.
- Propose a migration only when no existing column can carry the state, and state that trade-off in the PR description.

## When a migration is needed

- SQL files go in `crates/database/src/migrations/sql/`, named `V{NNNN}__{snake_case_description}.sql` with the next number. Refinery runs them on startup.
- The previous release keeps running against the migrated schema (see "Rolling deploys" in `.claude/rules/rust-correctness.md`). Add columns as nullable or defaulted, and remove or rename a column only after no running release reads it.
- Never lose data in a migration; backfills must be idempotent.

## Queries

- Use parameterized queries; never build SQL by formatting strings with input.
- Avoid queries inside loops (N+1); batch or join instead.
- A multi-step write that must succeed or fail together runs in one transaction.
- New query patterns on large tables need a supporting index.
