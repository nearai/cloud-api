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

/// Wall-clock milliseconds since the Unix epoch. A seam so tests can move time.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}
