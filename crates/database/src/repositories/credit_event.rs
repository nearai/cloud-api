use crate::models::{CreditClaim, CreditEvent, CreditEventCode};
use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use chrono::Utc;
use services::common::RepositoryError;
use tokio_postgres::Row;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct CreditEventRepository {
    pool: DbPool,
}

impl CreditEventRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    pub async fn create_event(
        &self,
        name: String,
        description: Option<String>,
        credit_amount: i64,
        currency: String,
        max_claims: Option<i32>,
        starts_at: chrono::DateTime<Utc>,
        claim_deadline: Option<chrono::DateTime<Utc>>,
        credit_expires_at: chrono::DateTime<Utc>,
        created_by_user_id: Option<Uuid>,
    ) -> Result<CreditEvent> {
        let row = retry_db!("create_credit_event", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let row = client
                .query_one(
                    r#"
                    INSERT INTO credit_events (
                        name, description, credit_amount, currency, max_claims,
                        starts_at, claim_deadline, credit_expires_at, created_by_user_id
                    ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                    RETURNING id, name, description, credit_amount, currency, max_claims,
                              claim_count, starts_at, claim_deadline, credit_expires_at,
                              is_active, created_by_user_id, created_at, updated_at
                    "#,
                    &[
                        &name,
                        &description,
                        &credit_amount,
                        &currency,
                        &max_claims,
                        &starts_at,
                        &claim_deadline,
                        &credit_expires_at,
                        &created_by_user_id,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            Ok::<tokio_postgres::Row, RepositoryError>(row)
        })?;

        Ok(self.row_to_credit_event(&row))
    }

    pub async fn get_event(&self, event_id: Uuid) -> Result<Option<CreditEvent>> {
        let result = retry_db!("get_credit_event", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    SELECT id, name, description, credit_amount, currency, max_claims,
                           claim_count, starts_at, claim_deadline, credit_expires_at,
                           is_active, created_by_user_id, created_at, updated_at
                    FROM credit_events
                    WHERE id = $1
                    "#,
                    &[&event_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(result.map(|row| self.row_to_credit_event(&row)))
    }

    pub async fn list_active_events(&self) -> Result<Vec<CreditEvent>> {
        let rows = retry_db!("list_active_credit_events", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    r#"
                    SELECT id, name, description, credit_amount, currency, max_claims,
                           claim_count, starts_at, claim_deadline, credit_expires_at,
                           is_active, created_by_user_id, created_at, updated_at
                    FROM credit_events
                    WHERE is_active = true
                    ORDER BY created_at DESC
                    "#,
                    &[],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(rows.iter().map(|r| self.row_to_credit_event(r)).collect())
    }

    pub async fn deactivate_event(&self, event_id: Uuid) -> Result<Option<CreditEvent>> {
        let result = retry_db!("deactivate_credit_event", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    UPDATE credit_events
                    SET is_active = false, updated_at = NOW()
                    WHERE id = $1 AND is_active = true
                    RETURNING id, name, description, credit_amount, currency, max_claims,
                              claim_count, starts_at, claim_deadline, credit_expires_at,
                              is_active, created_by_user_id, created_at, updated_at
                    "#,
                    &[&event_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(result.map(|row| self.row_to_credit_event(&row)))
    }

    pub async fn generate_codes(
        &self,
        event_id: Uuid,
        _count: i32,
        codes: Vec<String>,
    ) -> Result<Vec<CreditEventCode>> {
        let rows = retry_db!("generate_credit_event_codes", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let transaction = client.transaction().await.map_err(map_db_error)?;

            let event_exists = transaction
                .query_opt("SELECT 1 FROM credit_events WHERE id = $1", &[&event_id])
                .await
                .map_err(map_db_error)?;

            if event_exists.is_none() {
                return Err(RepositoryError::NotFound(format!(
                    "Credit event not found: {event_id}"
                )));
            }

            let mut result_rows = Vec::new();
            for code in &codes {
                let row = transaction
                    .query_one(
                        r#"
                        INSERT INTO credit_event_codes (credit_event_id, code)
                        VALUES ($1, $2)
                        RETURNING id, credit_event_id, code, is_claimed,
                                  claimed_by_user_id, claimed_by_near_account_id,
                                  claimed_at, created_at
                        "#,
                        &[&event_id, &code],
                    )
                    .await
                    .map_err(map_db_error)?;
                result_rows.push(row);
            }

            transaction.commit().await.map_err(map_db_error)?;

            Ok::<Vec<tokio_postgres::Row>, RepositoryError>(result_rows)
        })?;

        Ok(rows
            .iter()
            .map(|r| self.row_to_credit_event_code(r))
            .collect())
    }

    pub async fn get_codes_for_event(&self, event_id: Uuid) -> Result<Vec<CreditEventCode>> {
        let rows = retry_db!("get_credit_event_codes", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    r#"
                    SELECT id, credit_event_id, code, is_claimed,
                           claimed_by_user_id, claimed_by_near_account_id,
                           claimed_at, created_at
                    FROM credit_event_codes
                    WHERE credit_event_id = $1
                    ORDER BY created_at
                    "#,
                    &[&event_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(rows
            .iter()
            .map(|r| self.row_to_credit_event_code(r))
            .collect())
    }

    pub async fn find_unclaimed_code(
        &self,
        event_id: Uuid,
        code: &str,
    ) -> Result<Option<CreditEventCode>> {
        let result = retry_db!("find_unclaimed_credit_event_code", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    SELECT id, credit_event_id, code, is_claimed,
                           claimed_by_user_id, claimed_by_near_account_id,
                           claimed_at, created_at
                    FROM credit_event_codes
                    WHERE credit_event_id = $1 AND code = $2 AND is_claimed = false
                    "#,
                    &[&event_id, &code],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(result.map(|row| self.row_to_credit_event_code(&row)))
    }

    /// Atomically claim a promo code and increment the event's claim count
    /// within a single transaction. Returns appropriate errors if the code
    /// is already claimed or max_claims has been reached.
    pub async fn claim_code(
        &self,
        code_id: Uuid,
        event_id: Uuid,
        user_id: Uuid,
        near_account_id: &str,
        organization_id: Uuid,
        organization_limit_id: Option<Uuid>,
    ) -> Result<CreditClaim> {
        let row = retry_db!("claim_credit_event_code", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let transaction = client.transaction().await.map_err(map_db_error)?;

            // Step 1: Atomically increment claim_count, enforcing max_claims
            let incremented = transaction
                .execute(
                    r#"
                    UPDATE credit_events
                    SET claim_count = claim_count + 1, updated_at = NOW()
                    WHERE id = $1 AND is_active = true
                      AND (max_claims IS NULL OR claim_count < max_claims)
                    "#,
                    &[&event_id],
                )
                .await
                .map_err(map_db_error)?;

            if incremented == 0 {
                return Err(RepositoryError::ValidationFailed(
                    "Event is inactive or max claims reached".to_string(),
                ));
            }

            // Step 2: Mark code as claimed (fails if already claimed)
            let code_row = transaction
                .query_opt(
                    r#"
                    UPDATE credit_event_codes
                    SET is_claimed = true,
                        claimed_by_user_id = $1,
                        claimed_by_near_account_id = $2,
                        claimed_at = NOW()
                    WHERE id = $3 AND is_claimed = false
                    RETURNING id, credit_event_id, code, is_claimed,
                              claimed_by_user_id, claimed_by_near_account_id,
                              claimed_at, created_at
                    "#,
                    &[&user_id, &near_account_id, &code_id],
                )
                .await
                .map_err(map_db_error)?;

            if code_row.is_none() {
                return Err(RepositoryError::AlreadyExists);
            }

            // Step 3: Insert claim record
            let claim_row = transaction
                .query_one(
                    r#"
                    INSERT INTO credit_claims (
                        credit_event_id, code_id, near_account_id,
                        user_id, organization_id, organization_limit_id
                    ) VALUES ($1, $2, $3, $4, $5, $6)
                    RETURNING id, credit_event_id, code_id, near_account_id,
                              user_id, organization_id, organization_limit_id, claimed_at
                    "#,
                    &[
                        &code_row.as_ref().unwrap().get::<_, Uuid>("credit_event_id"),
                        &code_id,
                        &near_account_id,
                        &user_id,
                        &organization_id,
                        &organization_limit_id,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            transaction.commit().await.map_err(map_db_error)?;

            Ok::<tokio_postgres::Row, RepositoryError>(claim_row)
        })?;

        Ok(self.row_to_credit_claim(&row))
    }

    fn row_to_credit_event(&self, row: &Row) -> CreditEvent {
        CreditEvent {
            id: row.get("id"),
            name: row.get("name"),
            description: row.get("description"),
            credit_amount: row.get("credit_amount"),
            currency: row.get("currency"),
            max_claims: row.get("max_claims"),
            claim_count: row.get("claim_count"),
            starts_at: row.get("starts_at"),
            claim_deadline: row.get("claim_deadline"),
            credit_expires_at: row.get("credit_expires_at"),
            is_active: row.get("is_active"),
            created_by_user_id: row.get("created_by_user_id"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        }
    }

    fn row_to_credit_event_code(&self, row: &Row) -> CreditEventCode {
        CreditEventCode {
            id: row.get("id"),
            credit_event_id: row.get("credit_event_id"),
            code: row.get("code"),
            is_claimed: row.get("is_claimed"),
            claimed_by_user_id: row.get("claimed_by_user_id"),
            claimed_by_near_account_id: row.get("claimed_by_near_account_id"),
            claimed_at: row.get("claimed_at"),
            created_at: row.get("created_at"),
        }
    }

    fn row_to_credit_claim(&self, row: &Row) -> CreditClaim {
        CreditClaim {
            id: row.get("id"),
            credit_event_id: row.get("credit_event_id"),
            code_id: row.get("code_id"),
            near_account_id: row.get("near_account_id"),
            user_id: row.get("user_id"),
            organization_id: row.get("organization_id"),
            organization_limit_id: row.get("organization_limit_id"),
            claimed_at: row.get("claimed_at"),
        }
    }
}
