//! Crate-wide constants. Per the global constraints, all tunables and fixed
//! identifiers are code constants; there is exactly one runtime env var
//! (`PLACEMENT_REDIS_PASSWORD`, read outside this crate).

/// dstack event name under which inference-proxy records a replica report
/// signing key's public half. Matches inference-proxy's
/// `replica_state::report_key::REPORT_KEY_EVENT`.
pub const KEY_EVENT: &str = "nearai-replica-report-key-v1";

/// The only `ReplicaReport.schema` value this crate understands.
pub const SUPPORTED_SCHEMA: u8 = 1;

/// A replica's `engine_sampled_at_ms` is stale once it's older than this,
/// relative to now: 3x a 1s publish interval.
pub const FRESH_MAX_MS: u64 = 3_000;

/// KV cache usage at or above this fraction excludes a replica from routing.
pub const KV_MAX: f64 = 0.95;

/// Models eligible for smart routing; everything else falls back to
/// `Fleet::acquire_index`, unchanged. Confirm the exact model id in Task 12.
pub const COVERED_MODELS: &[&str] = &["zai-org/GLM-5.3-Flash"];
