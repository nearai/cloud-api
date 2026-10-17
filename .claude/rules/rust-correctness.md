---
paths:
  - "crates/**/*.rs"
---

# Rust correctness in production paths

These are the defect classes reviewers have repeatedly caught in this repo.

## Panics

- No `unwrap()`, `expect()`, unchecked indexing (`v[0]`), or byte-slicing of strings (`&s[..n]`) on paths reachable in production. Return a typed error instead. Tests and `const`/startup invariants with a comment explaining why they cannot fail are fine.
- Data parsed from outside the process (provider responses, attestation bundles, GitHub/Sigstore JSON, request bodies) is untrusted: check lengths and presence before indexing, and bound reads (`resp.bytes()` with no size cap, or an unbounded loop over a remote list, is a memory or time bomb).

## Fail closed

When a security or kill-switch decision depends on data that failed to load (settings read error, attestation fetch error, missing config), choose the safe outcome and log the failure. A transient error must never re-enable something an operator turned off.

## Async and resources

- No blocking I/O or CPU-heavy work directly on the async runtime; use `spawn_blocking`.
- Do not hold a `std::sync::Mutex` guard, or any lock, across `.await`.
- Every outbound call (HTTP, database, inference provider) has a timeout.
- Startup must not block on slow remote verification without a bound.
- Caches need an invalidation story and protection against stampedes when many requests miss at once.

## Concurrency and state

- Check-then-act sequences on shared state (provisioning, erasure, reload vs. PATCH) are races unless guarded by a transaction, a unique constraint, or a lock.
- A reload must not overwrite a newer write.
- Ownership checks compare both sides exactly (normalize case and whitespace before comparing, never after).

## Rolling deploys

Several releases run at once during a deploy, against the same database and clients. Anything the previous release reads must stay readable by it: wire formats, persisted JSON, enum serialization, API response shapes, and database schema (schema mechanics are in `.claude/rules/database.md`).

Before pushing, run the format, clippy, and dependency-guard commands listed under Commands in `CLAUDE.md`; CI runs the same ones.
