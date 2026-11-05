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
use super::ports::{ChannelRepository, PaymentClaimRepository};

/// Claims older than this are purged: a day, against a one-hour maximum challenge lifetime.
pub const CLAIM_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
/// Finalized channel rows contain only ledger state and expire after thirty days.
pub const CHANNEL_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

// A claim must outlive every credential that could still redeem its payment: the longest
// challenge plus the late-redemption window. Otherwise a purged claim could be spent again.
const _: () = assert!(
    config::MAX_MPP_CHALLENGE_TTL_SECS + super::service::LATE_REDEMPTION.as_secs()
        < CLAIM_RETENTION.as_secs()
);
/// How often the purge runs.
pub const PURGE_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Abandoned serving attempts become retryable after this age at the next hourly run.
pub const SERVING_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// One purge (many batches) ends here whatever the database does.
const PURGE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Release stale serving attempts; errors leave their claims closed until a later run.
pub async fn reclaim_once(repo: &dyn PaymentClaimRepository) -> Option<u64> {
    let Ok(age) = chrono::Duration::from_std(SERVING_TIMEOUT) else {
        return None;
    };
    match tokio::time::timeout(PURGE_TIMEOUT, repo.reclaim_stale_serving(Utc::now() - age)).await {
        Ok(Ok(reclaimed)) => {
            if reclaimed > 0 {
                tracing::info!(reclaimed, "MPP abandoned claims reclaimed");
            }
            Some(reclaimed)
        }
        Ok(Err(error)) => {
            tracing::warn!(
                outcome = "reclaim_failed",
                repository_error = repository_error_kind(&error),
                "MPP claim reclaim failed"
            );
            None
        }
        Err(_) => {
            tracing::warn!(outcome = "reclaim_timeout", "MPP claim reclaim timed out");
            None
        }
    }
}

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

/// Purge finalized channels with a bounded database call. Active channels are retained.
pub async fn purge_channels_once(repo: &dyn ChannelRepository) -> Option<u64> {
    let Ok(retention) = chrono::Duration::from_std(CHANNEL_RETENTION) else {
        return None;
    };
    match tokio::time::timeout(PURGE_TIMEOUT, repo.purge_finalized(Utc::now() - retention)).await {
        Ok(Ok(purged)) => {
            tracing::info!(purged, "MPP finalized channels purged");
            Some(purged)
        }
        Ok(Err(_)) => {
            tracing::warn!(outcome = "channel_purge_failed", "MPP channel purge failed");
            None
        }
        Err(_) => {
            tracing::warn!(
                outcome = "channel_purge_timeout",
                "MPP channel purge timed out"
            );
            None
        }
    }
}

/// Run alongside claim retention, only when sessions are configured.
pub fn spawn_channel_purge_task(repo: Arc<dyn ChannelRepository>) -> Option<JoinHandle<()>> {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::error!(outcome = "no_runtime", "MPP channel purge not started");
        return None;
    };
    Some(runtime.spawn(async move {
        let mut ticker = tokio::time::interval(PURGE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            purge_channels_once(repo.as_ref()).await;
        }
    }))
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
            reclaim_once(repo.as_ref()).await;
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
    async fn channel_purge_keeps_active_and_recent_finalized_rows() {
        use crate::mpp::{
            channel_memory::InMemoryChannelRepository, session::ChannelDescriptor, NewChannel,
        };
        let repo = InMemoryChannelRepository::new();
        for id in [1, 2, 3] {
            repo.open_or_refresh(NewChannel {
                channel_id: [id; 32],
                chain_id: 42431,
                escrow: [0xee; 20],
                descriptor: ChannelDescriptor {
                    payer: [1; 20],
                    payee: [2; 20],
                    operator: [0; 20],
                    token: [3; 20],
                    salt: [id; 32],
                    authorized_signer: [0; 20],
                    expiring_nonce_hash: [id; 32],
                },
                deposit: 100,
                settled_on_chain: 0,
                highest_cumulative: 100,
                highest_signature: [0; 65],
            })
            .await
            .unwrap();
        }
        repo.finalize(&[2; 32], 0).await.unwrap();
        repo.finalize(&[3; 32], 0).await.unwrap();
        repo.age_channel(&[1; 32], DAY * 31);
        repo.age_channel(&[2; 32], DAY * 31);
        repo.age_channel(&[3; 32], DAY * 29);
        assert_eq!(purge_channels_once(&repo).await, Some(1));
        assert!(repo.get(&[1; 32]).await.unwrap().is_some());
        assert!(repo.get(&[2; 32]).await.unwrap().is_none());
        assert!(repo.get(&[3; 32]).await.unwrap().is_some());
    }

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
    async fn the_hourly_task_reclaims_abandoned_serving_attempts() {
        let repo = Arc::new(InMemoryClaimRepository::default());
        repo.claim("abandoned", "c").await.unwrap();
        repo.age("abandoned", chrono::Duration::minutes(16));
        let task = spawn_purge_task_with(repo.clone(), Duration::from_millis(10), CLAIM_RETENTION)
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while repo.state_of("abandoned") != Some(crate::mpp::ClaimState::Unserved) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert_eq!(repo.len(), 1, "recovery retains the challenge binding");
    }

    #[tokio::test]
    async fn a_failing_store_is_reported_not_propagated() {
        let repo = FlakyClaimRepository::default();
        repo.set_fail_purges(true);
        assert_eq!(purge_once(&repo, CLAIM_RETENTION).await, None);
        assert_eq!(reclaim_once(&repo).await, None);
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
