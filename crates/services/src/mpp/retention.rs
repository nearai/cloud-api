//! Retention of the claim table.
//!
//! A claim only has to outlive the challenge it was made under (at most an hour, the largest
//! `MPP_CHALLENGE_TTL_SECS`): after that no credential for it can verify, so a replay is
//! refused by the HMAC and the expiry, not by the ledger. Rows older than
//! [`CLAIM_RETENTION`] are deleted by an hourly task. It logs counts and closed outcome
//! codes only.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::task::JoinHandle;

use super::error::repository_error_kind;
use super::ports::PaymentClaimRepository;

/// Claims older than this are purged: a day, against a one-hour maximum challenge lifetime.
pub const CLAIM_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

// A claim must outlive every credential that could still redeem its payment: the longest
// challenge plus the late-redemption window. Otherwise a purged claim could be spent again.
const _: () = assert!(
    config::MAX_MPP_CHALLENGE_TTL_SECS + super::service::LATE_REDEMPTION.as_secs()
        < CLAIM_RETENTION.as_secs()
);
/// How often the purge runs.
pub const PURGE_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// One purge (many batches) ends here whatever the database does.
const PURGE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Purge once; returns how many rows went, `None` on failure or timeout (already logged).
pub async fn purge_once(repo: &dyn PaymentClaimRepository, retention: Duration) -> Option<u64> {
    let Ok(retention) = chrono::Duration::from_std(retention) else {
        return None;
    };
    let cutoff = Utc::now() - retention;
    match tokio::time::timeout(PURGE_TIMEOUT, repo.purge_older_than(cutoff)).await {
        Ok(Ok(purged)) => {
            if purged > 0 {
                tracing::info!(purged, "MPP claims purged");
            }
            Some(purged)
        }
        Ok(Err(error)) => {
            tracing::warn!(
                outcome = "purge_failed",
                repository_error = repository_error_kind(&error),
                "MPP claim purge failed"
            );
            None
        }
        Err(_) => {
            tracing::warn!(outcome = "purge_timeout", "MPP claim purge timed out");
            None
        }
    }
}

/// Start the hourly purge. `None` (and an error log) when there is no async runtime.
pub fn spawn_purge_task(repo: Arc<dyn PaymentClaimRepository>) -> Option<JoinHandle<()>> {
    spawn_purge_task_with(repo, PURGE_INTERVAL, CLAIM_RETENTION)
}

/// [`spawn_purge_task`] with explicit timing (tests).
pub fn spawn_purge_task_with(
    repo: Arc<dyn PaymentClaimRepository>,
    interval: Duration,
    retention: Duration,
) -> Option<JoinHandle<()>> {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::error!(
            outcome = "no_runtime",
            "MPP claim purge not started: no async runtime"
        );
        return None;
    };
    Some(runtime.spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            purge_once(repo.as_ref(), retention).await;
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpp::memory::InMemoryClaimRepository;
    use crate::mpp::testing::FlakyClaimRepository;

    const DAY: chrono::Duration = chrono::Duration::hours(24);

    #[tokio::test]
    async fn purge_once_deletes_only_what_is_past_retention() {
        let repo = InMemoryClaimRepository::default();
        repo.claim("tempo:old", "c1").await.unwrap();
        repo.claim("tempo:new", "c2").await.unwrap();
        repo.age("tempo:old", DAY + chrono::Duration::minutes(1));
        repo.age("tempo:new", DAY - chrono::Duration::minutes(1));
        assert_eq!(purge_once(&repo, CLAIM_RETENTION).await, Some(1));
        assert_eq!(repo.len(), 1);
        assert_eq!(purge_once(&repo, CLAIM_RETENTION).await, Some(0));
    }

    #[tokio::test]
    async fn a_failing_store_is_reported_not_propagated() {
        let repo = FlakyClaimRepository::default();
        repo.set_fail_purges(true);
        assert_eq!(purge_once(&repo, CLAIM_RETENTION).await, None);
    }

    #[tokio::test]
    async fn the_task_runs_on_its_interval_and_survives_a_failing_run() {
        let repo = Arc::new(FlakyClaimRepository::default());
        for n in 0..3 {
            repo.claim(&format!("tempo:{n}"), "c").await.unwrap();
            repo.store().age(&format!("tempo:{n}"), DAY * 2);
        }
        repo.set_fail_purges(true);
        let task = spawn_purge_task_with(repo.clone(), Duration::from_millis(10), CLAIM_RETENTION)
            .expect("runtime");
        // The first runs fail; the task keeps going and purges once the store is back.
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(repo.store().len(), 3);
        repo.set_fail_purges(false);
        for _ in 0..200 {
            if repo.store().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(repo.store().is_empty(), "purged after the store recovered");
        task.abort();
    }

    #[test]
    fn no_runtime_means_no_task() {
        let repo: Arc<dyn PaymentClaimRepository> = Arc::new(InMemoryClaimRepository::default());
        assert!(spawn_purge_task(repo).is_none());
    }

    #[test]
    fn retention_outlives_the_longest_challenge() {
        // MPP_CHALLENGE_TTL_SECS is capped at one hour.
        assert!(CLAIM_RETENTION > Duration::from_secs(3600));
        assert!(PURGE_INTERVAL < CLAIM_RETENTION);
    }
}
