//! In-memory [`PaymentClaimRepository`]: the same state machine as the Postgres statement,
//! for unit tests and local harnesses. It forgets everything on restart and is per process,
//! so it must never back a deployment.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use super::ports::{ClaimOutcome, ClaimState, PaymentClaimRepository, ServeOutcome};
use crate::common::RepositoryError;

#[derive(Debug, Clone)]
struct Claim {
    challenge_id: String,
    state: ClaimState,
}

/// Claims keyed by payment key.
#[derive(Debug, Default)]
pub struct InMemoryClaimRepository {
    claims: Mutex<HashMap<String, Claim>>,
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
        let mut claims = self.claims.lock().map_err(|_| poisoned())?;
        match claims.get_mut(payment_key) {
            None => {
                claims.insert(
                    payment_key.to_string(),
                    Claim {
                        challenge_id: challenge_id.to_string(),
                        state: ClaimState::Serving,
                    },
                );
                Ok(ClaimOutcome::Claimed)
            }
            // A retry of a paid request whose serving failed, by the same challenge.
            Some(claim)
                if claim.state == ClaimState::Unserved && claim.challenge_id == challenge_id =>
            {
                claim.state = ClaimState::Serving;
                Ok(ClaimOutcome::Claimed)
            }
            Some(_) => Ok(ClaimOutcome::AlreadyUsed),
        }
    }

    async fn finish(
        &self,
        payment_key: &str,
        outcome: ServeOutcome,
    ) -> Result<(), RepositoryError> {
        let mut claims = self.claims.lock().map_err(|_| poisoned())?;
        if let Some(claim) = claims.get_mut(payment_key) {
            if claim.state == ClaimState::Serving {
                claim.state = outcome.state();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "tempo:0x1111111111111111111111111111111111111111111111111111111111111111";

    #[tokio::test]
    async fn first_claim_wins_and_a_replay_is_already_used() {
        let repo = InMemoryClaimRepository::default();
        assert_eq!(repo.claim(KEY, "c1").await.unwrap(), ClaimOutcome::Claimed);
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
        repo.claim(KEY, "c1").await.unwrap();
        repo.finish(KEY, ServeOutcome::Unserved).await.unwrap();
        assert_eq!(repo.state_of(KEY), Some(ClaimState::Unserved));
        assert_eq!(
            repo.claim(KEY, "c2").await.unwrap(),
            ClaimOutcome::AlreadyUsed
        );
        assert_eq!(repo.state_of(KEY), Some(ClaimState::Unserved));
        assert_eq!(repo.claim(KEY, "c1").await.unwrap(), ClaimOutcome::Claimed);
        assert_eq!(repo.state_of(KEY), Some(ClaimState::Serving));
    }

    #[tokio::test]
    async fn served_is_terminal() {
        let repo = InMemoryClaimRepository::default();
        repo.claim(KEY, "c1").await.unwrap();
        repo.finish(KEY, ServeOutcome::Served).await.unwrap();
        assert_eq!(repo.state_of(KEY), Some(ClaimState::Served));
        assert_eq!(
            repo.claim(KEY, "c1").await.unwrap(),
            ClaimOutcome::AlreadyUsed
        );
        // A late finish cannot reopen a served payment, nor flip it back.
        repo.finish(KEY, ServeOutcome::Unserved).await.unwrap();
        assert_eq!(repo.state_of(KEY), Some(ClaimState::Served));
    }

    #[tokio::test]
    async fn finish_only_moves_a_serving_row_and_ignores_unknown_keys() {
        let repo = InMemoryClaimRepository::default();
        repo.finish("tempo:0xunknown", ServeOutcome::Served)
            .await
            .unwrap();
        assert_eq!(repo.state_of("tempo:0xunknown"), None);
        repo.claim(KEY, "c1").await.unwrap();
        repo.finish(KEY, ServeOutcome::Unserved).await.unwrap();
        repo.finish(KEY, ServeOutcome::Served).await.unwrap();
        assert_eq!(repo.state_of(KEY), Some(ClaimState::Unserved));
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
            if task.await.unwrap() == ClaimOutcome::Claimed {
                claimed += 1;
            }
        }
        assert_eq!(claimed, 1);
    }
}
