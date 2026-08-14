use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use async_trait::async_trait;
use services::common::RepositoryError;
use services::completions::ports::{ConcurrencyLeaseRepository, HeldLease, LeaseOutcome};
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

    /// Zero and negative mean unset, as on the per-process path. Reading zero
    /// as a real limit would reject every request for the organization.
    fn effective_limit(stored: Option<i32>, default_limit: u32) -> u32 {
        stored
            .filter(|value| *value > 0)
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
            Ok(LeaseOutcome::Admitted { limit })
        })?;

        Ok(outcome)
    }

    async fn renew(&self, lease_ids: &[Uuid], ttl: Duration) -> Result<Vec<Uuid>> {
        if lease_ids.is_empty() {
            return Ok(Vec::new());
        }

        let ttl_seconds = ttl.as_secs() as f64;

        let rows = retry_db!("renew_concurrency_leases", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    r#"
                    UPDATE concurrency_leases
                    SET expires_at = NOW() + make_interval(secs => $2)
                    WHERE id = ANY($1)
                    RETURNING id
                    "#,
                    &[&lease_ids, &ttl_seconds],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(rows.iter().map(|row| row.get("id")).collect())
    }

    async fn persist(&self, leases: &[HeldLease], instance_id: &str, ttl: Duration) -> Result<()> {
        if leases.is_empty() {
            return Ok(());
        }

        let ids: Vec<Uuid> = leases.iter().map(|lease| lease.id).collect();
        let organization_ids: Vec<Uuid> =
            leases.iter().map(|lease| lease.organization_id).collect();
        let model_ids: Vec<Uuid> = leases.iter().map(|lease| lease.model_id).collect();
        let ttl_seconds = ttl.as_secs() as f64;

        retry_db!("persist_concurrency_leases", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute(
                    r#"
                    INSERT INTO concurrency_leases
                        (id, organization_id, model_id, instance_id, expires_at)
                    SELECT held.id, held.organization_id, held.model_id, $4,
                           NOW() + make_interval(secs => $5)
                    FROM UNNEST($1::uuid[], $2::uuid[], $3::uuid[])
                        AS held(id, organization_id, model_id)
                    ON CONFLICT (id) DO UPDATE SET expires_at = EXCLUDED.expires_at
                    "#,
                    &[
                        &ids,
                        &organization_ids,
                        &model_ids,
                        &instance_id,
                        &ttl_seconds,
                    ],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(())
    }

    async fn sweep_expired(&self) -> Result<u64> {
        let removed = retry_db!("sweep_expired_concurrency_leases", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            // Bounded so a backlog cannot become one long delete holding row
            // locks against live admissions; the next tick takes the rest.
            client
                .execute(
                    r#"
                    DELETE FROM concurrency_leases
                    WHERE id = ANY(
                        SELECT id FROM concurrency_leases
                        WHERE expires_at < NOW()
                        LIMIT 1000
                    )
                    "#,
                    &[],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(removed)
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
