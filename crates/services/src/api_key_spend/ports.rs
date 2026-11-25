//! Ports of the API-key spend check: where spend is read from, and the clock.

use async_trait::async_trait;
use uuid::Uuid;

/// The authoritative per-key spend, in the same units as the key's
/// `spend_limit` (the lifetime sum of the key's usage cost). Implemented over
/// the existing usage repository.
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
