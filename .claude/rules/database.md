---
paths:
  - "crates/database/**"
  - "crates/services/src/**/ports.rs"
---

# Database and migrations

PostgreSQL 16 behind Patroni, pooled with deadpool. Repositories in `crates/database/src/repositories/` implement the service-layer traits from `ports.rs`.

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
