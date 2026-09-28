//! Crate-wide constants. Per the global constraints, all tunables and fixed
//! identifiers are code constants; there is exactly one runtime env var
//! (`PLACEMENT_REDIS_PASSWORD`, read outside this crate).

/// dstack event name under which inference-proxy records a replica report
/// signing key's public half. Matches inference-proxy's
/// `replica_state::report_key::REPORT_KEY_EVENT`.
pub const KEY_EVENT: &str = "nearai-replica-report-key-v1";
