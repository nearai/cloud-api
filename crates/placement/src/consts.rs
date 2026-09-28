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

/// How far into the future a replica's `engine_sampled_at_ms` may be, relative
/// to this node's clock, before the frame is treated as clock skew. Such a
/// frame is rejected at ingest and never counts as fresh, so one bad clock step
/// on a GPU host cannot freeze that replica's view while it looks current.
pub const MAX_FUTURE_SKEW_MS: u64 = 2_000;

/// Prompts estimated above this many tokens need a long-context host.
pub const LONG_CONTEXT_TOKENS: u64 = 100_000;

/// KV cache usage at or above this fraction excludes a replica from routing.
pub const KV_MAX: f64 = 0.95;

/// Models eligible for smart routing; everything else falls back to
/// `Fleet::acquire_index`, unchanged.
///
/// Entries are catalog `model_name`s: the completions service rewrites
/// `params.model` to the catalog `model_name` before the pool, so this is what
/// `Fleet` sees. It must also equal the `model` inference-proxy signs into
/// every replica frame and key event (`rules::Rule::Model` and
/// `snapshot` compare them byte for byte). For GLM-5.3 Flash both are the
/// SGLang `--served-model-name` / inference-proxy `MODEL_NAME`,
/// `z-ai/glm-5.3-flash`, not the Hugging Face path `zai-org/GLM-5.3-Flash`.
pub const COVERED_MODELS: &[&str] = &["z-ai/glm-5.3-flash"];

/// Normalizer for a replica's prefill backlog (queued + pending tokens),
/// converting a token count into a `fullness`-comparable unit for `score.rs`.
pub const PREFILL_NORM_TOKENS: f64 = 16_000.0;

/// Fallback denominator for `fullness` when a replica reports no
/// `limits.max_running`.
pub const DEFAULT_MAX_RUNNING: f64 = 40.0;

/// The minimum speed multiplier `score.rs` divides by, so a replica with a
/// very low (or zero) `gen_tps` sample doesn't blow the score up to infinity.
pub const SPEED_FLOOR: f64 = 0.2;

/// Bounded-load affinity tolerance: stay on the affinity home/pin while its
/// score is within this fraction of the best eligible score.
pub const AFFINITY_EPS: f64 = 0.25;

/// Absolute slack added to the relative `AFFINITY_EPS` bound (see
/// `affinity::within_bound`), so affinity is not lost to a proportionally
/// large-looking gap when the whole fleet is near idle and `best` is close
/// to 0 (a relative-only bound would otherwise collapse to ~0 there).
pub const AFFINITY_ABS_SLACK: f64 = 0.1;

/// How long a follow pin stays valid after it's written, in milliseconds.
/// 10 minutes, matching OpenRouter's sticky-session TTL.
pub const PIN_TTL_MS: u64 = 600_000;

/// The synthetic replica id under which host-level `RoutedCounts` are keyed
/// in `Snapshot::routed`, since cloud-api's own routed counters are
/// host-level (the host's inference-proxy balances its own replicas).
pub const HOST_REPLICA: &str = "_host";
