//! In-memory [`PaymentClaimRepository`]: the same state machine as the Postgres statement,
//! for unit tests and local harnesses. It forgets everything on restart and is per process,
//! so it must never back a deployment.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::ports::{ClaimLease, ClaimOutcome, ClaimState, PaymentClaimRepository, ServeOutcome};
use crate::common::RepositoryError;

#[derive(Debug, Clone)]
struct Claim {
    challenge_id: String,
    state: ClaimState,
    created_at: DateTime<Utc>,
    /// Stands in for the row's `updated_at`: a new value on every write.
    lease: ClaimLease,
}

/// Claims keyed by payment key.
#[derive(Debug, Default)]
pub struct InMemoryClaimRepository {
    claims: Mutex<HashMap<String, Claim>>,
    writes: std::sync::atomic::AtomicI64,
}

impl InMemoryClaimRepository {
    /// A lease no earlier write has used, as `updated_at = NOW()` gives the real table.
    fn next_lease(&self) -> ClaimLease {
        let n = self
            .writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ClaimLease::new(DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::microseconds(n + 1))
    }
}

impl InMemoryClaimRepository {
    /// The state of `payment_key`, if claimed. For assertions.
    pub fn state_of(&self, payment_key: &str) -> Option<ClaimState> {
        self.claims
            .lock()
            .ok()
            .and_then(|claims| claims.get(payment_key).map(|claim| claim.state))
    }
}

impl InMemoryClaimRepository {
    /// Pretend `payment_key` was claimed `by` earlier. For purge tests.
    pub fn age(&self, payment_key: &str, by: chrono::Duration) {
        if let Ok(mut claims) = self.claims.lock() {
            if let Some(claim) = claims.get_mut(payment_key) {
                claim.created_at -= by;
            }
        }
    }

    /// Number of stored claims. For assertions.
    pub fn len(&self) -> usize {
        self.claims.lock().map(|claims| claims.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn poisoned() -> RepositoryError {
    RepositoryError::DatabaseError(anyhow::anyhow!("in-memory claim store poisoned"))
}

#[async_trait]
impl PaymentClaimRepository for InMemoryClaimRepository {
    async fn claim(
        &self,
        payment_key: &str,
        challenge_id: &str,
    ) -> Result<ClaimOutcome, RepositoryError> {
        let lease = self.next_lease();
        let mut claims = self.claims.lock().map_err(|_| poisoned())?;
        match claims.get_mut(payment_key) {
            None => {
                claims.insert(
                    payment_key.to_string(),
                    Claim {
                        challenge_id: challenge_id.to_string(),
                        state: ClaimState::Serving,
                        created_at: Utc::now(),
                        lease,
                    },
                );
                Ok(ClaimOutcome::Claimed(lease))
            }
            // A retry of a paid request whose serving failed, by the same challenge.
            Some(claim)
                if claim.state == ClaimState::Unserved && claim.challenge_id == challenge_id =>
            {
                claim.state = ClaimState::Serving;
                claim.lease = lease;
                Ok(ClaimOutcome::Claimed(lease))
            }
            Some(_) => Ok(ClaimOutcome::AlreadyUsed),
        }
    }

    async fn finish(
        &self,
        payment_key: &str,
        lease: ClaimLease,
        outcome: ServeOutcome,
    ) -> Result<(), RepositoryError> {
        let next = self.next_lease();
        let mut claims = self.claims.lock().map_err(|_| poisoned())?;
        if let Some(claim) = claims.get_mut(payment_key) {
            if claim.state == ClaimState::Serving && claim.lease == lease {
                claim.state = outcome.state();
                claim.lease = next;
            }
        }
        Ok(())
    }

    async fn purge_older_than(&self, cutoff: DateTime<Utc>) -> Result<u64, RepositoryError> {
        let mut claims = self.claims.lock().map_err(|_| poisoned())?;
        let before = claims.len();
        claims.retain(|_, claim| claim.created_at >= cutoff);
        Ok((before - claims.len()) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "tempo:0x1111111111111111111111111111111111111111111111111111111111111111";

    async fn claimed(repo: &InMemoryClaimRepository, key: &str, challenge: &str) -> ClaimLease {
        match repo.claim(key, challenge).await.unwrap() {
            ClaimOutcome::Claimed(lease) => lease,
            ClaimOutcome::AlreadyUsed => panic!("expected a claim"),
        }
    }

    #[tokio::test]
    async fn first_claim_wins_and_a_replay_is_already_used() {
        let repo = InMemoryClaimRepository::default();
        claimed(&repo, KEY, "c1").await;
        assert_eq!(repo.state_of(KEY), Some(ClaimState::Serving));
        // Replay while serving, by the same and by another challenge.
        assert_eq!(
            repo.claim(KEY, "c1").await.unwrap(),
            ClaimOutcome::AlreadyUsed
        );
        assert_eq!(
            repo.claim(KEY, "c2").await.unwrap(),
            ClaimOutcome::AlreadyUsed
        );
    }

    #[tokio::test]
    async fn unserved_is_reclaimable_only_by_the_same_challenge() {
        let repo = InMemoryClaimRepository::default();
        let lease = claimed(&repo, KEY, "c1").await;
        repo.finish(KEY, lease, ServeOutcome::Unserved)
            .await
            .unwrap();
        assert_eq!(repo.state_of(KEY), Some(ClaimState::Unserved));
        assert_eq!(
            repo.claim(KEY, "c2").await.unwrap(),
            ClaimOutcome::AlreadyUsed
        );
        assert_eq!(repo.state_of(KEY), Some(ClaimState::Unserved));
        let again = claimed(&repo, KEY, "c1").await;
        assert_ne!(again, lease, "every attempt gets its own lease");
        assert_eq!(repo.state_of(KEY), Some(ClaimState::Serving));
    }

    #[tokio::test]
    async fn served_is_terminal() {
        let repo = InMemoryClaimRepository::default();
        let lease = claimed(&repo, KEY, "c1").await;
        repo.finish(KEY, lease, ServeOutcome::Served).await.unwrap();
        assert_eq!(repo.state_of(KEY), Some(ClaimState::Served));
        assert_eq!(
            repo.claim(KEY, "c1").await.unwrap(),
            ClaimOutcome::AlreadyUsed
        );
        // A late finish cannot reopen a served payment, nor flip it back.
        repo.finish(KEY, lease, ServeOutcome::Unserved)
            .await
            .unwrap();
        assert_eq!(repo.state_of(KEY), Some(ClaimState::Served));
    }

    #[tokio::test]
    async fn a_stale_finish_cannot_release_a_newer_attempt() {
        // Attempt 1 fails and is released; its finish is retried (a lost reply) while the
        // client's retry, attempt 2, already holds the claim. The stale finish must not touch
        // attempt 2, or attempt 2's own finish would match nothing and leave the payment
        // reusable.
        let repo = InMemoryClaimRepository::default();
        let first = claimed(&repo, KEY, "c1").await;
        repo.finish(KEY, first, ServeOutcome::Unserved)
            .await
            .unwrap();
        let second = claimed(&repo, KEY, "c1").await;
        repo.finish(KEY, first, ServeOutcome::Unserved)
            .await
            .unwrap();
        assert_eq!(repo.state_of(KEY), Some(ClaimState::Serving));
        repo.finish(KEY, second, ServeOutcome::Served)
            .await
            .unwrap();
        assert_eq!(repo.state_of(KEY), Some(ClaimState::Served));
        assert_eq!(
            repo.claim(KEY, "c1").await.unwrap(),
            ClaimOutcome::AlreadyUsed
        );
    }

    #[tokio::test]
    async fn finish_only_moves_a_serving_row_and_ignores_unknown_keys() {
        let repo = InMemoryClaimRepository::default();
        let stranger = ClaimLease::new(Utc::now());
        repo.finish("tempo:0xunknown", stranger, ServeOutcome::Served)
            .await
            .unwrap();
        assert_eq!(repo.state_of("tempo:0xunknown"), None);
        let lease = claimed(&repo, KEY, "c1").await;
        repo.finish(KEY, lease, ServeOutcome::Unserved)
            .await
            .unwrap();
        repo.finish(KEY, lease, ServeOutcome::Served).await.unwrap();
        assert_eq!(repo.state_of(KEY), Some(ClaimState::Unserved));
    }

    #[tokio::test]
    async fn purge_removes_only_claims_older_than_the_cutoff() {
        let repo = InMemoryClaimRepository::default();
        let old = "tempo:old";
        claimed(&repo, old, "c1").await;
        claimed(&repo, KEY, "c2").await;
        repo.age(old, chrono::Duration::hours(25));
        let cutoff = Utc::now() - chrono::Duration::hours(24);
        assert_eq!(repo.purge_older_than(cutoff).await.unwrap(), 1);
        assert_eq!(repo.state_of(old), None);
        assert_eq!(repo.state_of(KEY), Some(ClaimState::Serving));
        assert_eq!(repo.purge_older_than(cutoff).await.unwrap(), 0);
        // A purged payment key could be claimed again: the table remembers a payment only as
        // long as its challenge can still be presented.
        claimed(&repo, old, "c3").await;
    }

    #[tokio::test]
    async fn concurrent_claims_have_exactly_one_winner() {
        let repo = std::sync::Arc::new(InMemoryClaimRepository::default());
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let repo = repo.clone();
            tasks.push(tokio::spawn(
                async move { repo.claim(KEY, "c1").await.unwrap() },
            ));
        }
        let mut claimed = 0;
        for task in tasks {
            if matches!(task.await.unwrap(), ClaimOutcome::Claimed(_)) {
                claimed += 1;
            }
        }
        assert_eq!(claimed, 1);
    }
}
