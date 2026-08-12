use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use async_trait::async_trait;
use services::common::RepositoryError;
use services::completions::ports::{ConcurrencyLeaseRepository, LeaseOutcome};
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct PostgresConcurrencyLeaseRepository {
    pool: DbPool,
}

impl PostgresConcurrencyLeaseRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    fn effective_limit(stored: Option<i32>, default_limit: u32) -> u32 {
        stored
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(default_limit)
    }
}

#[async_trait]
impl ConcurrencyLeaseRepository for PostgresConcurrencyLeaseRepository {
    async fn try_acquire(
        &self,
        lease_id: Uuid,
        organization_id: Uuid,
        model_id: Uuid,
        instance_id: &str,
        default_limit: u32,
        ttl: Duration,
    ) -> Result<LeaseOutcome> {
        let ttl_seconds = ttl.as_secs() as f64;
        let lock_key = format!("{organization_id}:{model_id}");

        let outcome = retry_db!("try_acquire_concurrency_lease", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;
            let tx = client.transaction().await.map_err(map_db_error)?;

            tx.execute(
                "SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))",
                &[&lock_key],
            )
            .await
            .map_err(map_db_error)?;

            let row = tx
                .query_one(
                    r#"
                    SELECT
                        (
                            SELECT rate_limit FROM organizations
                            WHERE id = $1 AND is_active = true
                        ) AS rate_limit,
                        (
                            SELECT COUNT(*) FROM concurrency_leases
                            WHERE organization_id = $1
                              AND model_id = $2
                              AND expires_at > NOW()
                        ) AS in_flight
                    "#,
                    &[&organization_id, &model_id],
                )
                .await
                .map_err(map_db_error)?;

            let limit =
                Self::effective_limit(row.get::<_, Option<i32>>("rate_limit"), default_limit);
            let in_flight: i64 = row.get("in_flight");

            if in_flight >= i64::from(limit) {
                tx.commit().await.map_err(map_db_error)?;
                return Ok::<LeaseOutcome, RepositoryError>(LeaseOutcome::AtLimit {
                    limit,
                    in_flight,
                });
            }

            tx.execute(
                r#"
                INSERT INTO concurrency_leases
                    (id, organization_id, model_id, instance_id, expires_at)
                VALUES ($1, $2, $3, $4, NOW() + make_interval(secs => $5))
                ON CONFLICT (id) DO NOTHING
                "#,
                &[
                    &lease_id,
                    &organization_id,
                    &model_id,
                    &instance_id,
                    &ttl_seconds,
                ],
            )
            .await
            .map_err(map_db_error)?;

            tx.commit().await.map_err(map_db_error)?;
            Ok(LeaseOutcome::Admitted)
        })?;

        Ok(outcome)
    }

    async fn release(&self, lease_ids: &[Uuid]) -> Result<()> {
        if lease_ids.is_empty() {
            return Ok(());
        }

        retry_db!("release_concurrency_leases", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute(
                    "DELETE FROM concurrency_leases WHERE id = ANY($1)",
                    &[&lease_ids],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(())
    }
}
