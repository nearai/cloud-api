use crate::retry_db;
use crate::{pool::DbPool, repositories::utils::map_db_error};
use anyhow::{Context, Result};
use async_trait::async_trait;
use services::common::RepositoryError;
use services::vector_stores::ports::{PaginationParams, VectorStoreRef, VectorStoreRefRepository};
use uuid::Uuid;

// ===========================================================================
// PgVectorStoreRefRepository — thin ref table for auth + pagination
// ===========================================================================

pub struct PgVectorStoreRefRepository {
    pool: DbPool,
}

impl PgVectorStoreRefRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    fn row_to_model(&self, row: tokio_postgres::Row) -> Result<VectorStoreRef> {
        Ok(VectorStoreRef {
            id: row.get("id"),
            workspace_id: row.get("workspace_id"),
            created_at: row.get("created_at"),
            deleted_at: row.get("deleted_at"),
        })
    }
}

#[async_trait]
impl VectorStoreRefRepository for PgVectorStoreRefRepository {
    async fn create(
        &self,
        id: Uuid,
        workspace_id: Uuid,
    ) -> Result<VectorStoreRef, RepositoryError> {
        let row = match retry_db!("create_vector_store_ref", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    "INSERT INTO vector_stores (id, workspace_id) VALUES ($1, $2) RETURNING *",
                    &[&id, &workspace_id],
                )
                .await
                .map_err(map_db_error)
        }) {
            Ok(row) => row,
            Err(RepositoryError::AlreadyExists) => {
                // Idempotent: fetch existing
                retry_db!("get_vector_store_ref_after_conflict", {
                    let client = self
                        .pool
                        .get()
                        .await
                        .context("Failed to get database connection")
                        .map_err(RepositoryError::PoolError)?;

                    client
                        .query_opt("SELECT * FROM vector_stores WHERE id = $1", &[&id])
                        .await
                        .map_err(map_db_error)
                })?
                .ok_or_else(|| {
                    RepositoryError::DatabaseError(anyhow::anyhow!(
                        "Vector store ref {id} conflict but not found"
                    ))
                })?
            }
            Err(e) => return Err(e),
        };

        self.row_to_model(row)
            .map_err(RepositoryError::DataConversionError)
    }

    async fn get(
        &self,
        id: Uuid,
        workspace_id: Uuid,
    ) -> Result<Option<VectorStoreRef>, RepositoryError> {
        let row = retry_db!("get_vector_store_ref", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    "SELECT * FROM vector_stores WHERE id = $1 AND workspace_id = $2 AND deleted_at IS NULL",
                    &[&id, &workspace_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        match row {
            Some(r) => Ok(Some(
                self.row_to_model(r)
                    .map_err(RepositoryError::DataConversionError)?,
            )),
            None => Ok(None),
        }
    }

    async fn list(
        &self,
        workspace_id: Uuid,
        params: &PaginationParams,
    ) -> Result<(Vec<VectorStoreRef>, bool), RepositoryError> {
        enum OrderDir {
            Asc,
            Desc,
        }
        let dir = if params.order == "asc" {
            OrderDir::Asc
        } else {
            OrderDir::Desc
        };
        let use_before = params.after.is_none() && params.before.is_some();
        let cursor_id = params.after.or(params.before);

        let (order_clause, comparison) = match (dir, use_before) {
            (OrderDir::Asc, false) | (OrderDir::Desc, true) => ("ASC", ">"),
            (OrderDir::Desc, false) | (OrderDir::Asc, true) => ("DESC", "<"),
        };

        // Fetch limit+1 to determine has_more
        let fetch_limit = (params.limit + 1) as i64;

        let query = match cursor_id {
            Some(_) => format!(
                "SELECT * FROM vector_stores
                 WHERE workspace_id = $1
                   AND deleted_at IS NULL
                   AND (created_at, id) {comparison} (SELECT created_at, id FROM vector_stores WHERE id = $2)
                 ORDER BY created_at {order_clause}, id {order_clause}
                 LIMIT $3"
            ),
            None => format!(
                "SELECT * FROM vector_stores
                 WHERE workspace_id = $1
                   AND deleted_at IS NULL
                 ORDER BY created_at {order_clause}, id {order_clause}
                 LIMIT $2"
            ),
        };

        let rows = retry_db!("list_vector_store_refs", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            match cursor_id {
                Some(cid) => {
                    client
                        .query(&query, &[&workspace_id, &cid, &fetch_limit])
                        .await
                }
                None => client.query(&query, &[&workspace_id, &fetch_limit]).await,
            }
            .map_err(map_db_error)
        })?;

        let mut results: Vec<VectorStoreRef> = rows
            .into_iter()
            .map(|row| {
                self.row_to_model(row)
                    .map_err(RepositoryError::DataConversionError)
            })
            .collect::<Result<Vec<_>, _>>()?;

        if use_before {
            results.reverse();
        }

        let has_more = results.len() > params.limit as usize;
        results.truncate(params.limit as usize);

        Ok((results, has_more))
    }

    async fn soft_delete(&self, id: Uuid, workspace_id: Uuid) -> Result<bool, RepositoryError> {
        let rows_affected = retry_db!("soft_delete_vector_store_ref", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute(
                    "UPDATE vector_stores SET deleted_at = NOW() WHERE id = $1 AND workspace_id = $2 AND deleted_at IS NULL",
                    &[&id, &workspace_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(rows_affected > 0)
    }
}
