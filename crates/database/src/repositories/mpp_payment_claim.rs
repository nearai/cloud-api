//! Single-use ledger of MPP payments (`mpp_payment_claims`).
//!
//! The table proves a payment was used once and stores nothing else: no organization,
//! workspace, request or payer. The schema is owned by `nearai/cloud-platform`
//! (`V0098__mpp_payment_claims.sql`); cloud-api only reads and writes it, with the
//! `cloud_api_responses` role.

use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::Context;
use async_trait::async_trait;
use services::common::RepositoryError;
use services::mpp::{ClaimOutcome, PaymentClaimRepository, ServeOutcome};

/// One atomic step: insert a new claim, or take over a row that an earlier attempt of the
/// *same challenge* left `unserved`. Any other existing row returns no row: already used.
const CLAIM_SQL: &str = "\
INSERT INTO mpp_payment_claims (payment_key, challenge_id, state) VALUES ($1, $2, 'serving') \
ON CONFLICT (payment_key) DO UPDATE SET state = 'serving', updated_at = NOW() \
WHERE mpp_payment_claims.state = 'unserved' \
AND mpp_payment_claims.challenge_id = EXCLUDED.challenge_id \
RETURNING payment_key";

/// Only a `serving` row moves, so a late or repeated `finish` can never reopen a served
/// payment. Idempotent, so it is safe to retry.
const FINISH_SQL: &str = "\
UPDATE mpp_payment_claims SET state = $2, updated_at = NOW() \
WHERE payment_key = $1 AND state = 'serving'";

pub struct MppPaymentClaimRepository {
    pool: DbPool,
}

impl MppPaymentClaimRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl PaymentClaimRepository for MppPaymentClaimRepository {
    /// Not retried: a retry after a commit whose reply was lost would see its own `serving`
    /// row and report the payment as used. The caller answers a failure with a 503 and the
    /// client presents the same credential again.
    async fn claim(
        &self,
        payment_key: &str,
        challenge_id: &str,
    ) -> Result<ClaimOutcome, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;
        let row = client
            .query_opt(CLAIM_SQL, &[&payment_key, &challenge_id])
            .await
            .map_err(map_db_error)?;
        Ok(if row.is_some() {
            ClaimOutcome::Claimed
        } else {
            ClaimOutcome::AlreadyUsed
        })
    }

    async fn finish(
        &self,
        payment_key: &str,
        outcome: ServeOutcome,
    ) -> Result<(), RepositoryError> {
        let state = outcome.state().as_str();
        retry_db!("finish_mpp_payment_claim", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;
            client
                .execute(FINISH_SQL, &[&payment_key, &state])
                .await
                .map_err(map_db_error)
                .map(|_| ())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use services::mpp::ClaimState;

    /// The literals in the statements are the closed state set; this fails if either side
    /// drifts from `ClaimState`.
    #[test]
    fn statements_use_the_claim_state_values() {
        assert!(CLAIM_SQL.contains(&format!("'{}'", ClaimState::Serving.as_str())));
        assert!(CLAIM_SQL.contains(&format!("= '{}'", ClaimState::Unserved.as_str())));
        assert!(FINISH_SQL.contains(&format!("state = '{}'", ClaimState::Serving.as_str())));
        // `claim` can only ever write `serving`; `served` is written by `finish` alone.
        assert!(!CLAIM_SQL.contains("'served'"));
        // Served and unserved are the only values `finish` can write, through a parameter.
        assert!(FINISH_SQL.contains("state = $2"));
    }

    /// Every statement is parameterized: no value is formatted into SQL.
    #[test]
    fn statements_bind_every_input() {
        for sql in [CLAIM_SQL, FINISH_SQL] {
            assert!(sql.contains("$1"));
            assert!(!sql.contains("tempo:"));
        }
        assert!(CLAIM_SQL.contains("$2"));
    }
}
