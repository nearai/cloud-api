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
use chrono::{DateTime, Utc};
use services::common::RepositoryError;
use services::mpp::{ClaimLease, ClaimOutcome, PaymentClaimRepository, ServeOutcome};

/// One atomic step: insert a new claim, or take over a row that an earlier attempt of the
/// *same challenge* left `unserved`. Any other existing row returns no row: already used.
/// The returned `updated_at` is the attempt's lease: every write sets it anew.
const CLAIM_SQL: &str = "\
INSERT INTO mpp_payment_claims (payment_key, challenge_id, state) VALUES ($1, $2, 'serving') \
ON CONFLICT (payment_key) DO UPDATE SET state = 'serving', updated_at = NOW() \
WHERE mpp_payment_claims.state = 'unserved' \
AND mpp_payment_claims.challenge_id = EXCLUDED.challenge_id \
RETURNING updated_at";

/// Only the `serving` row of this attempt (its lease) moves. A late or retried `finish` of
/// an earlier attempt matches nothing, so it can neither reopen a served payment nor release
/// a newer attempt's claim. Idempotent, so it is safe to retry.
const FINISH_SQL: &str = "\
UPDATE mpp_payment_claims SET state = $2, updated_at = NOW() \
WHERE payment_key = $1 AND state = 'serving' AND updated_at = $3";

/// A payment that can no longer be claimed under this challenge (see `CLAIM_SQL`): any row
/// except an `unserved` claim of the same challenge.
const SPENT_SQL: &str = "\
SELECT 1 FROM mpp_payment_claims WHERE payment_key = $1 \
AND NOT (state = 'unserved' AND challenge_id = $2)";

/// Rows deleted per statement: small enough that no statement holds its locks for long.
const PURGE_BATCH: i64 = 5000;
/// Statements per call. 200 batches is a million rows; anything left goes at the next call.
const PURGE_MAX_BATCHES: u32 = 200;

/// Oldest rows first, through `idx_mpp_payment_claims_created_at`. `SKIP LOCKED` lets two
/// instances purge at once without waiting on each other.
const PURGE_SQL: &str = "\
DELETE FROM mpp_payment_claims WHERE payment_key IN ( \
SELECT payment_key FROM mpp_payment_claims WHERE created_at < $1 \
ORDER BY created_at LIMIT $2 FOR UPDATE SKIP LOCKED)";

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
        Ok(match row {
            Some(row) => ClaimOutcome::Claimed(ClaimLease::new(
                row.try_get::<_, DateTime<Utc>>(0).map_err(map_db_error)?,
            )),
            None => ClaimOutcome::AlreadyUsed,
        })
    }

    async fn is_spent(
        &self,
        payment_key: &str,
        challenge_id: &str,
    ) -> Result<bool, RepositoryError> {
        retry_db!("is_mpp_payment_spent", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;
            client
                .query_opt(SPENT_SQL, &[&payment_key, &challenge_id])
                .await
                .map_err(map_db_error)
                .map(|row| row.is_some())
        })
    }

    async fn finish(
        &self,
        payment_key: &str,
        lease: ClaimLease,
        outcome: ServeOutcome,
    ) -> Result<(), RepositoryError> {
        let state = outcome.state().as_str();
        let lease = lease.at();
        retry_db!("finish_mpp_payment_claim", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;
            client
                .execute(FINISH_SQL, &[&payment_key, &state, &lease])
                .await
                .map_err(map_db_error)
                .map(|_| ())
        })
    }

    async fn purge_older_than(&self, cutoff: DateTime<Utc>) -> Result<u64, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;
        let mut purged = 0u64;
        for _ in 0..PURGE_MAX_BATCHES {
            let deleted = client
                .execute(PURGE_SQL, &[&cutoff, &PURGE_BATCH])
                .await
                .map_err(map_db_error)?;
            purged += deleted;
            if deleted < PURGE_BATCH as u64 {
                break;
            }
        }
        Ok(purged)
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
        for sql in [CLAIM_SQL, FINISH_SQL, PURGE_SQL] {
            assert!(sql.contains("$1"));
            assert!(!sql.contains("tempo:"));
        }
        assert!(CLAIM_SQL.contains("$2"));
        assert!(PURGE_SQL.contains("$2"));
    }

    /// The purge deletes by age through the `created_at` index, oldest first and bounded, and
    /// never touches a row another instance is already purging.
    #[test]
    fn purge_is_bounded_ordered_by_age_and_non_blocking() {
        assert!(PURGE_SQL.contains("WHERE created_at < $1"));
        assert!(PURGE_SQL.contains("ORDER BY created_at LIMIT $2"));
        assert!(PURGE_SQL.contains("SKIP LOCKED"));
        const _: () = assert!(PURGE_BATCH > 0 && PURGE_MAX_BATCHES > 0);
    }
}
