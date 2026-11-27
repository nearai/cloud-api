//! The per-request API-key spend-limit check. With the budget on, the check
//! is answered from a per-process budget ([`budget`]); with it off, every
//! check reads the database.

pub mod adapters;
mod budget;
#[cfg(test)]
mod budget_tests;
pub mod ports;
pub mod recorder;
#[cfg(test)]
mod recorder_tests;
#[cfg(test)]
mod test_support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::metrics::consts::{
    METRIC_API_KEY_SPEND_BUDGET_REFRESH, METRIC_API_KEY_SPEND_BUDGET_REFRESH_MS, TAG_RESULT,
};
use crate::metrics::MetricsServiceTrait;
use budget::Budget;
use ports::{ApiKeySpendSource, Clock};

/// Longest one database read may take; a read that exceeds it fails.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// What the spend check decided for one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Within the limit.
    Admit,
    /// The key has used the slice of headroom this process may spend between
    /// readings; answer 429 and tell the caller to retry after this many
    /// whole seconds (at least 1).
    Throttle { retry_after_secs: u64 },
    /// The key's spend has reached the limit.
    OverLimit { spend: i64 },
}

/// Why one database read failed.
enum ReadFailure {
    Timeout,
    Source(anyhow::Error),
}

impl ReadFailure {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Timeout => anyhow::anyhow!("API key spend read timed out"),
            Self::Source(e) => e,
        }
    }
}

/// One reading of a key's spend and when it was read (monotonic clock, taken
/// before the read started, so it never understates the age).
#[derive(Clone, Copy)]
struct Reading {
    spend: i64,
    read_at_ms: u64,
}

struct Inner {
    source: Arc<dyn ApiKeySpendSource>,
    metrics: Arc<dyn MetricsServiceTrait>,
    clock: Arc<dyn Clock>,
    /// Present when the budget is on.
    budget: Option<Budget>,
}

/// The spend lookup used by the API-key spend-limit check.
#[derive(Clone)]
pub struct ApiKeySpendCache {
    inner: Arc<Inner>,
}

impl ApiKeySpendCache {
    /// With `budget_max_age_secs` set, [`Self::check`] uses the budget (a
    /// reading older than that starts a refresh); without it every check
    /// reads `source`.
    pub fn new(
        source: Arc<dyn ApiKeySpendSource>,
        metrics: Arc<dyn MetricsServiceTrait>,
        clock: Arc<dyn Clock>,
        budget_max_age_secs: Option<u64>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                source,
                metrics,
                clock,
                budget: budget_max_age_secs.map(Budget::new),
            }),
        }
    }

    /// The decision for one request against `limit`. The error is the
    /// database read's, shared by every caller that was waiting on the same
    /// read.
    pub async fn check(
        &self,
        api_key_id: Uuid,
        limit: i64,
    ) -> Result<Decision, Arc<anyhow::Error>> {
        if let Some(budget) = &self.inner.budget {
            return self.check_budget(budget, api_key_id, limit).await;
        }
        let spend = self
            .inner
            .read(api_key_id)
            .await
            .map_err(|failure| Arc::new(failure.into_anyhow()))?
            .spend;
        Ok(if spend >= limit {
            Decision::OverLimit { spend }
        } else {
            Decision::Admit
        })
    }
}

impl Inner {
    fn count(&self, name: &str, tags: &[&str]) {
        self.metrics.record_count(name, 1, tags);
    }

    /// One database read bounded by [`READ_TIMEOUT`]. Every read is counted
    /// under the refresh metrics, tagged with its result.
    async fn read(&self, api_key_id: Uuid) -> Result<Reading, ReadFailure> {
        let read_at_ms = self.clock.now_ms();
        let started = Instant::now();
        let result =
            match tokio::time::timeout(READ_TIMEOUT, self.source.api_key_spend(api_key_id)).await {
                Ok(Ok(spend)) => Ok(Reading { spend, read_at_ms }),
                Ok(Err(e)) => Err(ReadFailure::Source(e)),
                Err(_) => Err(ReadFailure::Timeout),
            };
        self.metrics.record_histogram(
            METRIC_API_KEY_SPEND_BUDGET_REFRESH_MS,
            started.elapsed().as_secs_f64() * 1_000.0,
            &[],
        );
        let label = match &result {
            Ok(_) => "ok",
            Err(ReadFailure::Timeout) => "timeout",
            Err(ReadFailure::Source(_)) => "error",
        };
        let tag = format!("{TAG_RESULT}:{label}");
        self.count(METRIC_API_KEY_SPEND_BUDGET_REFRESH, &[tag.as_str()]);
        result
    }
}
