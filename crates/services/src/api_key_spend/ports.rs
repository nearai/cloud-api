//! Ports of the API-key spend cache: where spend is read from, and the clock.

use async_trait::async_trait;
use uuid::Uuid;

/// The authoritative per-key spend, in the same units as the key's
/// `spend_limit` (the lifetime sum of the key's usage cost). Implemented over
/// the existing usage repository; the cache calls it unchanged.
#[async_trait]
pub trait ApiKeySpendSource: Send + Sync {
    async fn api_key_spend(&self, api_key_id: Uuid) -> anyhow::Result<i64>;
}

/// Milliseconds on a monotonic clock. The value counts from an arbitrary
/// origin (the first reading, for [`SystemClock`]) and never goes backwards, even if
/// the wall clock is stepped; only differences between two readings are
/// meaningful. A seam so tests can move time.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// Milliseconds elapsed since the first use of the clock in this process,
/// measured with [`std::time::Instant`].
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        ORIGIN
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_millis() as u64
    }
}

/// Why a call to the shared total store failed. A closed set: it is the only
/// thing about a store failure that is counted or logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreErrorKind {
    Timeout,
    /// The store's ACL forbids the command or key (also when a function
    /// touches a key it may not).
    NoPerm,
    /// The library with the functions is not loaded on the store.
    NoFunction,
    Conn,
    Other,
}

impl StoreErrorKind {
    /// Metric tag value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::NoPerm => "noperm",
            Self::NoFunction => "no_function",
            Self::Conn => "conn",
            Self::Other => "other",
        }
    }
}

/// What the store holds as a key's running total.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TotalRead {
    /// A canonical, non-negative decimal `i64`.
    Value(i64),
    Missing,
    /// Present but not a canonical non-negative decimal `i64` (counted by the
    /// caller, then treated as missing).
    Malformed,
}

/// The shared running spend total of a key, kept in a store that is shared by
/// all processes (Valkey). Best-effort: the total is maintained by
/// increments, so it is never authoritative and every failure is survivable.
///
/// The protocol (see the module docs): `begin`, then the database sum, then
/// `finish` seeds a missing total; `add` is sent after each usage row commits;
/// `invalidate` drops the total when it is known to be wrong.
#[async_trait]
pub trait SpendTotalStore: Send + Sync {
    async fn get_total(&self, api_key_id: Uuid) -> Result<TotalRead, StoreErrorKind>;

    /// Starts a seed: from now on costs are also collected as pending. Returns
    /// the generation token to hand to [`Self::finish`]. Tokens are random, so
    /// one never repeats after the store lost its data.
    async fn begin(&self, api_key_id: Uuid) -> Result<String, StoreErrorKind>;

    /// Adds a committed cost to the total and to the pending costs, each only
    /// if it exists (never creates a key). Returns a bit set: 1 = total
    /// updated, 2 = pending updated.
    async fn add(&self, api_key_id: Uuid, cost: i64) -> Result<u8, StoreErrorKind>;

    /// Writes `sum` plus the pending costs as the total, only if the
    /// generation token still equals `expected_generation`, the pending costs
    /// exist and there is no total. Returns the new total, or `None` when it
    /// aborted.
    async fn finish(
        &self,
        api_key_id: Uuid,
        sum: i64,
        expected_generation: &str,
    ) -> Result<Option<i64>, StoreErrorKind>;

    /// Drops the total and the pending costs and replaces the generation
    /// token, so a seed in flight aborts.
    async fn invalidate(&self, api_key_id: Uuid) -> Result<(), StoreErrorKind>;
}
