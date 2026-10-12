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

/// Unique index that keeps a verified contact address to one user.
const VERIFIED_CONTACT_EMAIL_INDEX: &str = "idx_users_contact_email_verified";

pub struct PgContactEmailRepository {
    pool: DbPool,
}

impl PgContactEmailRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

/// Seconds to wait when `max` or more verification emails matching `filter` (a
/// predicate over `user_contact_email_verifications`, with `key` as `$3` when it
/// needs one) were sent in the last `window_secs`. `None` when under the limit.
///
/// The wait ends when enough counted sends have left the window for the count
/// to be under `max` again. With `count` sends that is when the
/// `(count - max + 1)`-th oldest leaves: the oldest one when the limit is just
/// reached, a later one when the count is over it (the global cap can be
/// overshot, or lowered while sends are still in the window).
async fn rate_limit_wait<C: GenericClient + Sync>(
    client: &C,
    filter: &str,
    key: Option<&(dyn tokio_postgres::types::ToSql + Sync)>,
    max: i64,
    window_secs: i64,
) -> Result<Option<u64>, RepositoryError> {
    // A limit of zero is never under its maximum: no send leaving the window
    // restores capacity, so the wait is the whole window.
    if max <= 0 {
        return Ok(Some(window_secs.max(1) as u64));
    }
    let window = window_secs as f64;
    let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = vec![&window, &max];
    params.extend(key);
    let row = client
        .query_opt(
            &format!(
                "SELECT EXTRACT(EPOCH FROM created_at + make_interval(secs => $1) - NOW())::float8 AS wait_secs \
                 FROM (SELECT created_at, \
                              COUNT(*) OVER () AS sends, \
                              ROW_NUMBER() OVER (ORDER BY created_at) AS position \
                       FROM user_contact_email_verifications \
                       WHERE {filter} AND created_at > NOW() - make_interval(secs => $1)) counted \
                 WHERE position = sends - $2 + 1"
            ),
            &params,
        )
        .await
        .map_err(map_db_error)?;
    Ok(row.map(|row| row.get::<_, f64>("wait_secs").ceil().max(1.0) as u64))
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
            // address (by its limit key) across users. With both held, the
            // per-user and per-address counts below are exact. The global cap is
            // a backstop and is not serialized: requests for different addresses
            // running at the same moment can overshoot it by a few sends.
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
                    "SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))",
                    &[&verification.email_limit_key],
                )
                .await
                .map_err(map_db_error)?;

            let per_user = rate_limit_wait(
                &*transaction,
                "user_id = $3",
                Some(&verification.user_id),
                limits.per_user,
                limits.per_user_window_secs,
            )
            .await?;
            let global = rate_limit_wait(
                &*transaction,
                "TRUE",
                None,
                limits.global,
                limits.global_window_secs,
            )
            .await?;
            if let Some(retry_after_secs) = per_user.max(global) {
                transaction.rollback().await.map_err(map_db_error)?;
                return Ok(CreateVerificationOutcome::RateLimited { retry_after_secs });
            }
            // Checked last and reported apart: the caller is not told about it.
            let per_address = rate_limit_wait(
                &*transaction,
                "email_limit_key = $3",
                Some(&verification.email_limit_key),
                limits.per_address,
                limits.per_address_window_secs,
            )
            .await?;
            if per_address.is_some() {
                transaction.rollback().await.map_err(map_db_error)?;
                return Ok(CreateVerificationOutcome::AddressLimitReached);
            }

            // clock_timestamp(), not NOW(): rows of one user are inserted one at
            // a time under the user row lock, and "earlier" must follow that
            // order even when a transaction started before the one it waited for.
            let row = transaction
                .query_one(
                    "INSERT INTO user_contact_email_verifications \
                         (user_id, email, email_limit_key, token_hash, expires_at, created_at) \
                     VALUES ($1, $2, $3, $4, $5, clock_timestamp()) RETURNING id",
                    &[
                        &verification.user_id,
                        &verification.email,
                        &verification.email_limit_key,
                        &verification.token_hash,
                        &verification.expires_at,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            transaction.commit().await.map_err(map_db_error)?;
            Ok(CreateVerificationOutcome::Created {
                verification_id: row.get("id"),
            })
        })?;
        Ok(outcome)
    }

    async fn expire_earlier_verifications(
        &self,
        user_id: Uuid,
        verification_id: Uuid,
    ) -> Result<()> {
        retry_db!("expire_earlier_contact_email_verifications", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;
            let transaction = client.transaction().await.map_err(map_db_error)?;

            // Same lock order as requests and confirmations: the user row first.
            transaction
                .query_opt(
                    "SELECT 1 FROM users WHERE id = $1 FOR NO KEY UPDATE",
                    &[&user_id],
                )
                .await
                .map_err(map_db_error)?;
            // The rows stay, because the send limits count them.
            transaction
                .execute(
                    "UPDATE user_contact_email_verifications SET expires_at = NOW() \
                     WHERE user_id = $1 AND id <> $2 AND consumed_at IS NULL AND expires_at > NOW() \
                       AND created_at < (SELECT created_at FROM user_contact_email_verifications \
                                         WHERE id = $2)",
                    &[&user_id, &verification_id],
                )
                .await
                .map_err(map_db_error)?;

            transaction.commit().await.map_err(map_db_error)
        })?;
        Ok(())
    }

    async fn delete_verification(&self, verification_id: Uuid) -> Result<()> {
        retry_db!("delete_contact_email_verification", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;
            client
                .execute(
                    "DELETE FROM user_contact_email_verifications WHERE id = $1",
                    &[&verification_id],
                )
                .await
                .map_err(map_db_error)
        })?;
        Ok(())
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
                    "SELECT v.email, v.expires_at, v.consumed_at, clock_timestamp() AS now, \
                            NOT EXISTS (SELECT 1 FROM user_contact_email_verifications newer \
                                        WHERE newer.user_id = v.user_id \
                                          AND newer.created_at > v.created_at) AS is_newest \
                     FROM user_contact_email_verifications v \
                     WHERE v.token_hash = $1 FOR UPDATE OF v",
                    &[&token_hash],
                )
                .await
                .map_err(map_db_error)?
            else {
                transaction.rollback().await.map_err(map_db_error)?;
                return Ok(ConfirmOutcome::InvalidToken);
            };
            // Only the user's newest link can confirm. Earlier ones are normally
            // expired once the newer email is sent, but that step runs after the
            // send and may not have happened; this check does not depend on it.
            // A send that failed deleted its row, so the link before it is the
            // newest again.
            if !stored.get::<_, bool>("is_newest")
                || !verification_is_usable(
                    stored.get("expires_at"),
                    stored.get("consumed_at"),
                    stored.get("now"),
                )
            {
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
                // Only that index means "address in use". Any other unique
                // violation is a database error like any other.
                Err(err)
                    if err.as_db_error().is_some_and(|db| {
                        db.code() == &SqlState::UNIQUE_VIOLATION
                            && db.constraint() == Some(VERIFIED_CONTACT_EMAIL_INDEX)
                    }) =>
                {
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

    async fn remove_contact_email(&self, user_id: Uuid) -> Result<bool> {
        let removed = retry_db!("remove_contact_email", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;
            let transaction = client.transaction().await.map_err(map_db_error)?;

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
                return Ok(false);
            }
            // Leaves the row untouched when there is nothing to clear, so a
            // repeated call does not move `updated_at`.
            transaction
                .execute(
                    "UPDATE users SET contact_email = NULL, contact_email_verified_at = NULL, \
                     email_opt_in_at = NULL, updated_at = NOW() \
                     WHERE id = $1 AND (contact_email IS NOT NULL \
                                        OR contact_email_verified_at IS NOT NULL \
                                        OR email_opt_in_at IS NOT NULL)",
                    &[&user_id],
                )
                .await
                .map_err(map_db_error)?;
            transaction
                .execute(
                    "UPDATE user_contact_email_verifications SET expires_at = NOW() \
                     WHERE user_id = $1 AND consumed_at IS NULL AND expires_at > NOW()",
                    &[&user_id],
                )
                .await
                .map_err(map_db_error)?;

            transaction.commit().await.map_err(map_db_error)?;
            Ok(true)
        })?;
        Ok(removed)
    }
}
