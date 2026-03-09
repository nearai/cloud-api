use crate::models::Service;
use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use services::common::RepositoryError;
use tokio_postgres::Row;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ServiceRepository {
    pool: DbPool,
}

impl ServiceRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Get service by id (any active or inactive).
    pub async fn get_by_id(&self, id: Uuid) -> Result<Option<Service>> {
        let rows = retry_db!("get_service_by_id", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    r#"
                    SELECT id, service_name, display_name, description, unit, cost_per_unit,
                           is_active, created_at, updated_at
                    FROM services
                    WHERE id = $1
                    "#,
                    &[&id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(rows.first().map(|row| self.row_to_service(row)))
    }

    /// Get active service by service_name (for billing lookup).
    pub async fn get_active_by_name(&self, service_name: &str) -> Result<Option<Service>> {
        let rows = retry_db!("get_active_service_by_name", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    r#"
                    SELECT id, service_name, display_name, description, unit, cost_per_unit,
                           is_active, created_at, updated_at
                    FROM services
                    WHERE service_name = $1 AND is_active = true
                    "#,
                    &[&service_name],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(rows.first().map(|row| self.row_to_service(row)))
    }

    /// List services with pagination. When include_inactive is false, only active.
    pub async fn list(
        &self,
        include_inactive: bool,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<Service>, i64)> {
        let (rows, total) = retry_db!("list_services", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let count_sql = if include_inactive {
                "SELECT COUNT(*)::BIGINT FROM services"
            } else {
                "SELECT COUNT(*)::BIGINT FROM services WHERE is_active = true"
            };
            let total: i64 = client
                .query_one(count_sql, &[])
                .await
                .map_err(map_db_error)?
                .get(0);

            let where_clause = if include_inactive {
                ""
            } else {
                " WHERE is_active = true"
            };
            let rows = client
                .query(
                    &format!(
                        r#"
                        SELECT id, service_name, display_name, description, unit, cost_per_unit,
                               is_active, created_at, updated_at
                        FROM services{}
                        ORDER BY service_name ASC
                        LIMIT $1 OFFSET $2
                        "#,
                        where_clause
                    ),
                    &[&limit, &offset],
                )
                .await
                .map_err(map_db_error)?;

            Ok::<_, RepositoryError>((rows, total))
        })?;

        let services = rows.iter().map(|row| self.row_to_service(row)).collect();
        Ok((services, total))
    }

    /// Create a new service.
    pub async fn create(
        &self,
        service_name: &str,
        display_name: &str,
        description: Option<&str>,
        unit: &str,
        cost_per_unit: i64,
    ) -> Result<Service> {
        let row = retry_db!("create_service", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    r#"
                    INSERT INTO services (service_name, display_name, description, unit, cost_per_unit)
                    VALUES ($1, $2, $3, $4, $5)
                    RETURNING id, service_name, display_name, description, unit, cost_per_unit,
                              is_active, created_at, updated_at
                    "#,
                    &[&service_name, &display_name, &description, &unit, &cost_per_unit],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(self.row_to_service(&row))
    }

    /// Update display_name, description, cost_per_unit, is_active (service_name and unit immutable).
    /// None means leave the column unchanged.
    pub async fn update(
        &self,
        id: Uuid,
        display_name: Option<&str>,
        description: Option<&str>,
        cost_per_unit: Option<i64>,
        is_active: Option<bool>,
    ) -> Result<Option<Service>> {
        let row = retry_db!("update_service", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    UPDATE services
                    SET
                        display_name = COALESCE($2, (SELECT display_name FROM services WHERE id = $1)),
                        description = COALESCE($3, (SELECT description FROM services WHERE id = $1)),
                        cost_per_unit = COALESCE($4, (SELECT cost_per_unit FROM services WHERE id = $1)),
                        is_active = COALESCE($5, (SELECT is_active FROM services WHERE id = $1)),
                        updated_at = NOW()
                    WHERE id = $1
                    RETURNING id, service_name, display_name, description, unit, cost_per_unit,
                              is_active, created_at, updated_at
                    "#,
                    &[&id, &display_name, &description, &cost_per_unit, &is_active],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(row.map(|r| self.row_to_service(&r)))
    }

    fn row_to_service(&self, row: &Row) -> Service {
        Service {
            id: row.get("id"),
            service_name: row.get("service_name"),
            display_name: row.get("display_name"),
            description: row.get("description"),
            unit: row.get("unit"),
            cost_per_unit: row.get("cost_per_unit"),
            is_active: row.get("is_active"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        }
    }
}
