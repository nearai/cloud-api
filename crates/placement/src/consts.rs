//! Crate-wide constants. Per the global constraints, all tunables and fixed
//! identifiers are code constants; there is exactly one runtime env var
//! (`PLACEMENT_REDIS_PASSWORD`, read outside this crate).

/// dstack event name under which inference-proxy records a replica report
/// signing key's public half. Matches inference-proxy's
/// `replica_state::report_key::REPORT_KEY_EVENT`.
pub const KEY_EVENT: &str = "nearai-replica-report-key-v1";

/// The only `HostReport.schema` value this crate understands.
pub const SUPPORTED_SCHEMA: u8 = 1;

/// The most replicas one host frame may carry. A larger frame is rejected by
/// `frame::open`, bounding what a single host can add to a snapshot.
pub const MAX_REPLICAS_PER_HOST: usize = 64;

/// A replica's `engine_sampled_at_ms` is stale once it's older than this,
/// relative to now: 3x a 1s publish interval.
pub const FRESH_MAX_MS: u64 = 3_000;

/// How far into the future a replica's `engine_sampled_at_ms` may be, relative
/// to this node's clock, before the frame is treated as clock skew. Such a
/// frame is rejected at ingest and never counts as fresh, so one bad clock step
/// on a GPU host cannot freeze that replica's view while it looks current.
pub const MAX_FUTURE_SKEW_MS: u64 = 2_000;

/// KV cache usage at or above this fraction excludes a replica from routing.
pub const KV_MAX: f64 = 0.95;

/// Normalizer for a replica's prefill backlog (queued + pending tokens),
/// converting a token count into a `fullness`-comparable unit for `score.rs`.
pub const PREFILL_NORM_TOKENS: f64 = 16_000.0;

/// Fallback denominator for `fullness` when a replica reports no
/// `limits.max_running`.
pub const DEFAULT_MAX_RUNNING: f64 = 40.0;

/// The minimum speed multiplier `score.rs` divides by, so a replica with a
/// very low (or zero, while streams are running) per-stream `gen_tps` doesn't
/// blow the score up to infinity.
pub const SPEED_FLOOR: f64 = 0.2;

/// Bounded-load affinity tolerance: stay on the affinity home/pin while its
/// score is within this fraction of the best eligible score.
pub const AFFINITY_EPS: f64 = 0.25;

/// Absolute slack added to the relative `AFFINITY_EPS` bound (see
/// `affinity::within_bound`), in score units.
///
/// One unit is `PREFILL_NORM_TOKENS` (16K) prefill-equivalent tokens, or a
/// full `max_running` of extra streams. Ordinary jitter between replicas is
/// well inside that: gpu03 runs 8 vs 12 of 32 streams side by side (~0.25
/// apart), and a short 3-deep queue (~3K backlog) is ~0.2. A warm prefix
/// saves its whole length in prefill on a return, so spilling it for less
/// than ~16K tokens of extra wait is a net loss. At 0.1 (about 1.6K tokens,
/// or 3 streams) the bound spilled warm conversations on that jitter. It
/// also keeps affinity when the fleet is near idle and `best` is close to 0,
/// where the relative bound alone collapses to ~0.
pub const AFFINITY_ABS_SLACK: f64 = 1.0;

/// How long a follow pin stays valid after it's written, in milliseconds.
/// 10 minutes, matching OpenRouter's sticky-session TTL.
pub const PIN_TTL_MS: u64 = 600_000;

// Heavy lane (see `policy`). There is deliberately no class-line constant:
// whether a request is heavy is the pool's decision, from its declared tier
// capacities, and arrives as `PlaceInput::prefill_heavy`.

/// The largest share of a Fleet's live replicas that may be heavy-lane
/// members: `lane_cap = ceil(live * HEAVY_SHARE)`.
pub const HEAVY_SHARE: f64 = 0.25;

/// A replica whose load (prefill backlog + pending tokens) is at least this
/// is a heavy-lane member, whatever the class of the requests that put the
/// load there.
pub const LANE_LOAD_TOKENS: u64 = 64_000;

/// A heavy request is admitted to a base-tier replica only while that
/// replica's load plus the request's prompt stays at or under this.
pub const HEAVY_BACKLOG_CAP: u64 = 300_000;

/// The largest prompt a base-tier replica admits as heavy work, whatever its
/// load. Base engines accept up to 1M context, so without this a prompt that
/// overflows the long tier would put a 500K+ prefill on a base replica and
/// undo the long tier's isolation. Anything larger finds no candidate on base
/// (`LegacyReason::LaneFull`). Below `HEAVY_BACKLOG_CAP`, so the base lane has
/// no idle waiver: an idle replica always fits one prompt this size.
pub const HEAVY_BASE_MAX_PROMPT: u64 = 200_000;

/// The same admission cap for long-tier replicas, which prefill faster.
pub const LONG_BACKLOG_CAP: u64 = 600_000;
