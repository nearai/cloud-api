use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use async_trait::async_trait;
use services::common::RepositoryError;
use services::contact_email::{
    verification_is_usable, ConfirmOutcome, ContactEmailRepository, CreateVerificationOutcome,
    NewVerification, SendLimits,
};
use tokio_postgres::error::SqlState;
use tokio_postgres::GenericClient;
use uuid::Uuid;

pub struct PgContactEmailRepository {
    pool: DbPool,
}

impl PgContactEmailRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

/// Seconds to wait when `max` or more verification emails matching `filter` (a
/// predicate over `user_contact_email_verifications` with `$1` as its only
/// parameter) were sent in the last `window_secs`. `None` when under the limit.
/// The wait ends when the oldest counted send leaves the window.
async fn rate_limit_wait<C: GenericClient + Sync>(
    client: &C,
    filter: &str,
    key: &(dyn tokio_postgres::types::ToSql + Sync),
    max: i64,
    window_secs: i64,
) -> Result<Option<u64>, RepositoryError> {
    let window_secs = window_secs as f64;
    let row = client
        .query_one(
            &format!(
                "SELECT COUNT(*) AS sends, \
                        EXTRACT(EPOCH FROM MIN(created_at) + make_interval(secs => $2) - NOW())::float8 AS wait_secs \
                 FROM user_contact_email_verifications \
                 WHERE {filter} AND created_at > NOW() - make_interval(secs => $2)"
            ),
            &[key, &window_secs],
        )
        .await
        .map_err(map_db_error)?;
    if row.get::<_, i64>("sends") < max {
        return Ok(None);
    }
    let wait_secs: Option<f64> = row.get("wait_secs");
    Ok(Some(wait_secs.unwrap_or(0.0).ceil().max(1.0) as u64))
}

#[async_trait]
impl ContactEmailRepository for PgContactEmailRepository {
    async fn create_verification(
        &self,
        verification: NewVerification<'_>,
        limits: SendLimits,
    ) -> Result<CreateVerificationOutcome> {
        let outcome = retry_db!("create_contact_email_verification", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;
            let transaction = client.transaction().await.map_err(map_db_error)?;

            // The user row lock makes this user's requests and confirmations run
            // one at a time. The advisory lock does the same for requests to one
            // address across users. With both held, the counts below are exact.
            if transaction
                .query_opt(
                    "SELECT 1 FROM users WHERE id = $1 AND is_active = true FOR NO KEY UPDATE",
                    &[&verification.user_id],
                )
                .await
                .map_err(map_db_error)?
                .is_none()
            {
                transaction.rollback().await.map_err(map_db_error)?;
                return Ok(CreateVerificationOutcome::UserNotFound);
            }
            transaction
                .query(
                    "SELECT pg_advisory_xact_lock(hashtextextended(lower($1::text), 0))",
                    &[&verification.email],
                )
                .await
                .map_err(map_db_error)?;

            let per_user = rate_limit_wait(
                &*transaction,
                "user_id = $1",
                &verification.user_id,
                limits.per_user,
                limits.per_user_window_secs,
            )
            .await?;
            let per_address = rate_limit_wait(
                &*transaction,
                "lower(email) = lower($1::text)",
                &verification.email,
                limits.per_address,
                limits.per_address_window_secs,
            )
            .await?;
            if let Some(retry_after_secs) = per_user.max(per_address) {
                transaction.rollback().await.map_err(map_db_error)?;
                return Ok(CreateVerificationOutcome::RateLimited { retry_after_secs });
            }

            // A new link replaces the user's earlier unused ones. The rows stay,
            // because the limits above count them.
            transaction
                .execute(
                    "UPDATE user_contact_email_verifications SET expires_at = NOW() \
                     WHERE user_id = $1 AND consumed_at IS NULL AND expires_at > NOW()",
                    &[&verification.user_id],
                )
                .await
                .map_err(map_db_error)?;
            transaction
                .execute(
                    "INSERT INTO user_contact_email_verifications (user_id, email, token_hash, expires_at) \
                     VALUES ($1, $2, $3, $4)",
                    &[
                        &verification.user_id,
                        &verification.email,
                        &verification.token_hash,
                        &verification.expires_at,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            transaction.commit().await.map_err(map_db_error)?;
            Ok(CreateVerificationOutcome::Created)
        })?;
        Ok(outcome)
    }

    async fn confirm(&self, token_hash: &str) -> Result<ConfirmOutcome> {
        let outcome = retry_db!("confirm_contact_email", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;
            let transaction = client.transaction().await.map_err(map_db_error)?;

            // Find the owner without a lock, so the user row can be locked before
            // the verification row. Requests lock in the same order, which rules
            // out a deadlock between asking for a new link and opening an old one.
            let Some(owner) = transaction
                .query_opt(
                    "SELECT user_id FROM user_contact_email_verifications WHERE token_hash = $1",
                    &[&token_hash],
                )
                .await
                .map_err(map_db_error)?
            else {
                transaction.rollback().await.map_err(map_db_error)?;
                return Ok(ConfirmOutcome::InvalidToken);
            };
            let user_id: Uuid = owner.get("user_id");
            if transaction
                .query_opt(
                    "SELECT 1 FROM users WHERE id = $1 AND is_active = true FOR NO KEY UPDATE",
                    &[&user_id],
                )
                .await
                .map_err(map_db_error)?
                .is_none()
            {
                transaction.rollback().await.map_err(map_db_error)?;
                return Ok(ConfirmOutcome::InvalidToken);
            }

            // clock_timestamp(), not NOW(): this transaction may have started
            // before a newer request expired the token and then waited for it.
            let Some(stored) = transaction
                .query_opt(
                    "SELECT email, expires_at, consumed_at, clock_timestamp() AS now \
                     FROM user_contact_email_verifications WHERE token_hash = $1 FOR UPDATE",
                    &[&token_hash],
                )
                .await
                .map_err(map_db_error)?
            else {
                transaction.rollback().await.map_err(map_db_error)?;
                return Ok(ConfirmOutcome::InvalidToken);
            };
            if !verification_is_usable(
                stored.get("expires_at"),
                stored.get("consumed_at"),
                stored.get("now"),
            ) {
                transaction.rollback().await.map_err(map_db_error)?;
                return Ok(ConfirmOutcome::InvalidToken);
            }
            let email: String = stored.get("email");

            // The address must not be another user's login email or verified
            // contact email. Deactivated users still own their addresses.
            let taken = transaction
                .query_opt(
                    "SELECT 1 FROM users \
                     WHERE id <> $1 \
                       AND (lower(email) = lower($2::text) \
                            OR (contact_email_verified_at IS NOT NULL \
                                AND lower(contact_email) = lower($2::text))) \
                     LIMIT 1",
                    &[&user_id, &email],
                )
                .await
                .map_err(map_db_error)?;
            if taken.is_some() {
                transaction.rollback().await.map_err(map_db_error)?;
                return Ok(ConfirmOutcome::AddressInUse { user_id });
            }

            transaction
                .execute(
                    "UPDATE user_contact_email_verifications SET consumed_at = NOW() WHERE token_hash = $1",
                    &[&token_hash],
                )
                .await
                .map_err(map_db_error)?;
            // Two users confirming the same address at the same time both pass the
            // check above. The unique index on the verified address then lets only
            // one of them commit; the other is rolled back as a conflict.
            match transaction
                .execute(
                    "UPDATE users SET contact_email = $2, contact_email_verified_at = NOW(), \
                     email_opt_in_at = NOW(), updated_at = NOW() WHERE id = $1",
                    &[&user_id, &email],
                )
                .await
            {
                Ok(_) => {}
                Err(err) if err.code() == Some(&SqlState::UNIQUE_VIOLATION) => {
                    transaction.rollback().await.map_err(map_db_error)?;
                    return Ok(ConfirmOutcome::AddressInUse { user_id });
                }
                Err(err) => return Err(map_db_error(err)),
            }

            transaction.commit().await.map_err(map_db_error)?;
            Ok(ConfirmOutcome::Verified { user_id })
        })?;
        Ok(outcome)
    }

    async fn delete_older_than(&self, max_age_secs: i64) -> Result<u64> {
        let max_age_secs = max_age_secs as f64;
        let deleted = retry_db!("delete_old_contact_email_verifications", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;
            client
                .execute(
                    "DELETE FROM user_contact_email_verifications \
                     WHERE created_at < NOW() - make_interval(secs => $1)",
                    &[&max_age_secs],
                )
                .await
                .map_err(map_db_error)
        })?;
        Ok(deleted)
    }
}
