use crate::retry_db;
use crate::{pool::DbPool, repositories::utils::map_db_error};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use services::common::RepositoryError;
use services::vector_stores::{
    CreateVectorStoreFileBatchParams, CreateVectorStoreFileParams, CreateVectorStoreParams,
    ListParams, UpdateVectorStoreParams, VectorStore, VectorStoreFile, VectorStoreFileBatch,
    VectorStoreFileBatchRepository, VectorStoreFileRepository, VectorStoreRepository,
};
use uuid::Uuid;

// ===========================================================================
// PgVectorStoreRepository
// ===========================================================================

pub struct PgVectorStoreRepository {
    pool: DbPool,
}

impl PgVectorStoreRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    fn row_to_model(&self, row: tokio_postgres::Row) -> Result<VectorStore> {
        Ok(VectorStore {
            id: row.get("id"),
            workspace_id: row.get("workspace_id"),
            name: row.get("name"),
            description: row.get("description"),
            status: row.get("status"),
            usage_bytes: row.get("usage_bytes"),
            file_counts_in_progress: row.get("file_counts_in_progress"),
            file_counts_completed: row.get("file_counts_completed"),
            file_counts_failed: row.get("file_counts_failed"),
            file_counts_cancelled: row.get("file_counts_cancelled"),
            file_counts_total: row.get("file_counts_total"),
            last_active_at: row.get("last_active_at"),
            expires_after_anchor: row.get("expires_after_anchor"),
            expires_after_days: row.get("expires_after_days"),
            expires_at: row.get("expires_at"),
            metadata: row.get("metadata"),
            chunking_strategy: row.get("chunking_strategy"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
            deleted_at: row.get("deleted_at"),
        })
    }

    pub async fn create(
        &self,
        params: CreateVectorStoreParams,
    ) -> Result<VectorStore, RepositoryError> {
        let id = Uuid::new_v4();
        let metadata = params.metadata.unwrap_or_else(|| serde_json::json!({}));
        let chunking_strategy = params
            .chunking_strategy
            .unwrap_or_else(|| serde_json::json!({"type": "auto"}));

        let row = match retry_db!("create_vector_store", {
            let now = Utc::now();
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    r#"
                    INSERT INTO vector_stores (
                        id, workspace_id, name, description,
                        expires_after_anchor, expires_after_days,
                        metadata, chunking_strategy, created_at, updated_at, last_active_at
                    )
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9, $9)
                    RETURNING *
                    "#,
                    &[
                        &id,
                        &params.workspace_id,
                        &params.name,
                        &params.description,
                        &params.expires_after_anchor,
                        &params.expires_after_days,
                        &metadata,
                        &chunking_strategy,
                        &now,
                    ],
                )
                .await
                .map_err(map_db_error)
        }) {
            Ok(row) => row,
            Err(RepositoryError::AlreadyExists) => retry_db!("get_vector_store_after_conflict", {
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
                    "Vector store {id} was reported as existing but not found"
                ))
            })?,
            Err(e) => return Err(e),
        };

        self.row_to_model(row)
            .map_err(RepositoryError::DataConversionError)
    }

    pub async fn get_by_id_and_workspace(
        &self,
        id: Uuid,
        workspace_id: Uuid,
    ) -> Result<Option<VectorStore>, RepositoryError> {
        let row = retry_db!("get_vector_store_by_id_and_workspace", {
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

    pub async fn list(&self, params: &ListParams) -> Result<Vec<VectorStore>, RepositoryError> {
        let is_asc = params.order == "asc";

        // For `before` cursor, we query in reverse order and flip results after
        let use_before = params.after.is_none() && params.before.is_some();
        let cursor_id = params.after.or(params.before);

        // When using `before`, reverse sort direction to fetch closest items first
        let order_clause = match (is_asc, use_before) {
            (true, false) | (false, true) => "ASC",
            (false, false) | (true, true) => "DESC",
        };
        let comparison = match (is_asc, use_before) {
            (true, false) | (false, true) => ">",
            (false, false) | (true, true) => "<",
        };

        let query = match cursor_id {
            Some(_) => format!(
                "SELECT * FROM vector_stores
                 WHERE workspace_id = $1
                   AND deleted_at IS NULL
                   AND created_at {comparison} (SELECT created_at FROM vector_stores WHERE id = $2)
                 ORDER BY created_at {order_clause}
                 LIMIT $3"
            ),
            None => format!(
                "SELECT * FROM vector_stores
                 WHERE workspace_id = $1
                   AND deleted_at IS NULL
                 ORDER BY created_at {order_clause}
                 LIMIT $2"
            ),
        };

        let rows = retry_db!("list_vector_stores", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            match cursor_id {
                Some(cid) => {
                    client
                        .query(&query, &[&params.workspace_id, &cid, &params.limit])
                        .await
                }
                None => {
                    client
                        .query(&query, &[&params.workspace_id, &params.limit])
                        .await
                }
            }
            .map_err(map_db_error)
        })?;

        let mut results: Vec<VectorStore> = rows
            .into_iter()
            .map(|row| {
                self.row_to_model(row)
                    .map_err(RepositoryError::DataConversionError)
            })
            .collect::<Result<Vec<_>, _>>()?;

        // Reverse results when using `before` cursor to restore original sort order
        if use_before {
            results.reverse();
        }

        Ok(results)
    }

    pub async fn update(
        &self,
        id: Uuid,
        workspace_id: Uuid,
        params: &UpdateVectorStoreParams,
    ) -> Result<Option<VectorStore>, RepositoryError> {
        let row = retry_db!("update_vector_store", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    UPDATE vector_stores SET
                        name = COALESCE($3, name),
                        expires_after_anchor = CASE WHEN $4::boolean THEN $5 ELSE expires_after_anchor END,
                        expires_after_days = CASE WHEN $6::boolean THEN $7 ELSE expires_after_days END,
                        metadata = CASE WHEN $8::boolean THEN $9 ELSE metadata END,
                        last_active_at = NOW()
                    WHERE id = $1 AND workspace_id = $2 AND deleted_at IS NULL
                    RETURNING *
                    "#,
                    &[
                        &id,
                        &workspace_id,
                        &params.name,
                        &params.expires_after_anchor.is_some(),
                        &params.expires_after_anchor,
                        &params.expires_after_days.is_some(),
                        &params.expires_after_days,
                        &params.metadata.is_some(),
                        &params.metadata.as_ref().unwrap_or(&serde_json::json!({})),
                    ],
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

    pub async fn soft_delete(&self, id: Uuid, workspace_id: Uuid) -> Result<bool, RepositoryError> {
        let rows_affected = retry_db!("soft_delete_vector_store", {
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

    pub async fn update_file_counts(&self, id: Uuid) -> Result<(), RepositoryError> {
        retry_db!("update_vector_store_file_counts", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute(
                    r#"
                    UPDATE vector_stores SET
                        file_counts_in_progress = (SELECT COUNT(*) FROM vector_store_files WHERE vector_store_id = $1 AND status = 'in_progress'),
                        file_counts_completed   = (SELECT COUNT(*) FROM vector_store_files WHERE vector_store_id = $1 AND status = 'completed'),
                        file_counts_failed      = (SELECT COUNT(*) FROM vector_store_files WHERE vector_store_id = $1 AND status = 'failed'),
                        file_counts_cancelled   = (SELECT COUNT(*) FROM vector_store_files WHERE vector_store_id = $1 AND status = 'cancelled'),
                        file_counts_total       = (SELECT COUNT(*) FROM vector_store_files WHERE vector_store_id = $1),
                        usage_bytes             = COALESCE((SELECT SUM(usage_bytes) FROM vector_store_files WHERE vector_store_id = $1), 0),
                        last_active_at          = NOW()
                    WHERE id = $1
                    "#,
                    &[&id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(())
    }
}

#[async_trait]
impl VectorStoreRepository for PgVectorStoreRepository {
    async fn create(
        &self,
        params: CreateVectorStoreParams,
    ) -> Result<VectorStore, RepositoryError> {
        self.create(params).await
    }

    async fn get_by_id_and_workspace(
        &self,
        id: Uuid,
        workspace_id: Uuid,
    ) -> Result<Option<VectorStore>, RepositoryError> {
        self.get_by_id_and_workspace(id, workspace_id).await
    }

    async fn list(&self, params: &ListParams) -> Result<Vec<VectorStore>, RepositoryError> {
        self.list(params).await
    }

    async fn update(
        &self,
        id: Uuid,
        workspace_id: Uuid,
        params: &UpdateVectorStoreParams,
    ) -> Result<Option<VectorStore>, RepositoryError> {
        self.update(id, workspace_id, params).await
    }

    async fn soft_delete(&self, id: Uuid, workspace_id: Uuid) -> Result<bool, RepositoryError> {
        self.soft_delete(id, workspace_id).await
    }

    async fn update_file_counts(&self, id: Uuid) -> Result<(), RepositoryError> {
        self.update_file_counts(id).await
    }
}

// ===========================================================================
// PgVectorStoreFileRepository
// ===========================================================================

pub struct PgVectorStoreFileRepository {
    pool: DbPool,
}

impl PgVectorStoreFileRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    fn row_to_model(&self, row: tokio_postgres::Row) -> Result<VectorStoreFile> {
        Ok(VectorStoreFile {
            id: row.get("id"),
            vector_store_id: row.get("vector_store_id"),
            file_id: row.get("file_id"),
            workspace_id: row.get("workspace_id"),
            batch_id: row.get("batch_id"),
            status: row.get("status"),
            usage_bytes: row.get("usage_bytes"),
            chunk_count: row.get("chunk_count"),
            chunking_strategy: row.get("chunking_strategy"),
            attributes: row.get("attributes"),
            last_error: row.get("last_error"),
            created_at: row.get("created_at"),
            processing_started_at: row.get("processing_started_at"),
            processing_completed_at: row.get("processing_completed_at"),
            updated_at: row.get("updated_at"),
        })
    }

    pub async fn create(
        &self,
        params: CreateVectorStoreFileParams,
    ) -> Result<VectorStoreFile, RepositoryError> {
        let id = Uuid::new_v4();
        let attributes = params.attributes.unwrap_or_else(|| serde_json::json!({}));

        // Files are set to completed immediately (no actual processing in this PR)
        let row = match retry_db!("create_vector_store_file", {
            let now = Utc::now();
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    r#"
                    INSERT INTO vector_store_files (
                        id, vector_store_id, file_id, workspace_id, batch_id,
                        status, chunking_strategy, attributes,
                        created_at, processing_started_at, processing_completed_at, updated_at
                    )
                    VALUES ($1, $2, $3, $4, $5, 'completed', $6, $7, $8, $8, $8, $8)
                    RETURNING *
                    "#,
                    &[
                        &id,
                        &params.vector_store_id,
                        &params.file_id,
                        &params.workspace_id,
                        &params.batch_id,
                        &params.chunking_strategy,
                        &attributes,
                        &now,
                    ],
                )
                .await
                .map_err(map_db_error)
        }) {
            Ok(row) => row,
            Err(RepositoryError::AlreadyExists) => {
                return Err(RepositoryError::AlreadyExists);
            }
            Err(e) => return Err(e),
        };

        self.row_to_model(row)
            .map_err(RepositoryError::DataConversionError)
    }

    pub async fn get(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<Option<VectorStoreFile>, RepositoryError> {
        let row = retry_db!("get_vector_store_file", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    "SELECT * FROM vector_store_files WHERE id = $1 AND vector_store_id = $2 AND workspace_id = $3",
                    &[&id, &vector_store_id, &workspace_id],
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

    /// Build a paginated query for vector_store_files with cursor, filter, and sort support.
    ///
    /// `base_conditions` are pre-existing WHERE clauses (e.g. "vector_store_id = $1 AND workspace_id = $2").
    /// `base_params` are the corresponding parameter values.
    /// Returns `(query_string, params, use_before)` where `use_before` indicates results need reversing.
    fn build_file_list_query(
        base_conditions: &str,
        base_params: Vec<Box<dyn tokio_postgres::types::ToSql + Sync + Send>>,
        params: &ListParams,
    ) -> (
        String,
        Vec<Box<dyn tokio_postgres::types::ToSql + Sync + Send>>,
        bool,
    ) {
        let is_asc = params.order == "asc";
        let use_before = params.after.is_none() && params.before.is_some();
        let cursor_id = params.after.or(params.before);

        // When using `before`, reverse sort direction to fetch closest items first
        let order_clause = match (is_asc, use_before) {
            (true, false) | (false, true) => "ASC",
            (false, false) | (true, true) => "DESC",
        };
        let comparison = match (is_asc, use_before) {
            (true, false) | (false, true) => ">",
            (false, false) | (true, true) => "<",
        };

        let mut query_params = base_params;
        let mut extra_conditions = String::new();

        if let Some(filter) = params.filter.as_ref() {
            let idx = query_params.len() + 1;
            extra_conditions.push_str(&format!(" AND status = ${idx}"));
            query_params.push(Box::new(filter.clone()));
        }

        if let Some(cid) = cursor_id {
            let idx = query_params.len() + 1;
            extra_conditions.push_str(&format!(
                " AND created_at {comparison} (SELECT created_at FROM vector_store_files WHERE id = ${idx})"
            ));
            query_params.push(Box::new(cid));
        }

        let limit_idx = query_params.len() + 1;
        query_params.push(Box::new(params.limit));

        let query = format!(
            "SELECT * FROM vector_store_files WHERE {base_conditions}{extra_conditions} ORDER BY created_at {order_clause} LIMIT ${limit_idx}"
        );

        (query, query_params, use_before)
    }

    pub async fn list(
        &self,
        vector_store_id: Uuid,
        workspace_id: Uuid,
        params: &ListParams,
    ) -> Result<Vec<VectorStoreFile>, RepositoryError> {
        let base_params: Vec<Box<dyn tokio_postgres::types::ToSql + Sync + Send>> = vec![
            Box::new(vector_store_id),
            Box::new(workspace_id),
        ];
        let (query, query_params, use_before) = Self::build_file_list_query(
            "vector_store_id = $1 AND workspace_id = $2",
            base_params,
            params,
        );

        let param_refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = query_params
            .iter()
            .map(|p| p.as_ref() as &(dyn tokio_postgres::types::ToSql + Sync))
            .collect();

        let rows = retry_db!("list_vector_store_files", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(&query, &param_refs)
                .await
                .map_err(map_db_error)
        })?;

        let mut results: Vec<VectorStoreFile> = rows
            .into_iter()
            .map(|row| {
                self.row_to_model(row)
                    .map_err(RepositoryError::DataConversionError)
            })
            .collect::<Result<Vec<_>, _>>()?;

        if use_before {
            results.reverse();
        }

        Ok(results)
    }

    pub async fn update_attributes(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
        attributes: serde_json::Value,
    ) -> Result<Option<VectorStoreFile>, RepositoryError> {
        let row = retry_db!("update_vector_store_file_attributes", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    UPDATE vector_store_files SET attributes = $4
                    WHERE id = $1 AND vector_store_id = $2 AND workspace_id = $3
                    RETURNING *
                    "#,
                    &[&id, &vector_store_id, &workspace_id, &attributes],
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

    pub async fn delete(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<bool, RepositoryError> {
        let rows_affected = retry_db!("delete_vector_store_file", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute(
                    "DELETE FROM vector_store_files WHERE id = $1 AND vector_store_id = $2 AND workspace_id = $3",
                    &[&id, &vector_store_id, &workspace_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(rows_affected > 0)
    }

    pub async fn list_by_batch(
        &self,
        batch_id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
        params: &ListParams,
    ) -> Result<Vec<VectorStoreFile>, RepositoryError> {
        let base_params: Vec<Box<dyn tokio_postgres::types::ToSql + Sync + Send>> = vec![
            Box::new(batch_id),
            Box::new(vector_store_id),
            Box::new(workspace_id),
        ];
        let (query, query_params, use_before) = Self::build_file_list_query(
            "batch_id = $1 AND vector_store_id = $2 AND workspace_id = $3",
            base_params,
            params,
        );

        let param_refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = query_params
            .iter()
            .map(|p| p.as_ref() as &(dyn tokio_postgres::types::ToSql + Sync))
            .collect();

        let rows = retry_db!("list_vector_store_files_by_batch", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(&query, &param_refs)
                .await
                .map_err(map_db_error)
        })?;

        let mut results: Vec<VectorStoreFile> = rows
            .into_iter()
            .map(|row| {
                self.row_to_model(row)
                    .map_err(RepositoryError::DataConversionError)
            })
            .collect::<Result<Vec<_>, _>>()?;

        if use_before {
            results.reverse();
        }

        Ok(results)
    }
}

#[async_trait]
impl VectorStoreFileRepository for PgVectorStoreFileRepository {
    async fn create(
        &self,
        params: CreateVectorStoreFileParams,
    ) -> Result<VectorStoreFile, RepositoryError> {
        self.create(params).await
    }

    async fn get(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<Option<VectorStoreFile>, RepositoryError> {
        self.get(id, vector_store_id, workspace_id).await
    }

    async fn list(
        &self,
        vector_store_id: Uuid,
        workspace_id: Uuid,
        params: &ListParams,
    ) -> Result<Vec<VectorStoreFile>, RepositoryError> {
        self.list(vector_store_id, workspace_id, params).await
    }

    async fn update_attributes(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
        attributes: serde_json::Value,
    ) -> Result<Option<VectorStoreFile>, RepositoryError> {
        self.update_attributes(id, vector_store_id, workspace_id, attributes)
            .await
    }

    async fn delete(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<bool, RepositoryError> {
        self.delete(id, vector_store_id, workspace_id).await
    }

    async fn list_by_batch(
        &self,
        batch_id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
        params: &ListParams,
    ) -> Result<Vec<VectorStoreFile>, RepositoryError> {
        self.list_by_batch(batch_id, vector_store_id, workspace_id, params)
            .await
    }
}

// ===========================================================================
// PgVectorStoreFileBatchRepository
// ===========================================================================

pub struct PgVectorStoreFileBatchRepository {
    pool: DbPool,
}

impl PgVectorStoreFileBatchRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    fn row_to_model(&self, row: tokio_postgres::Row) -> Result<VectorStoreFileBatch> {
        Ok(VectorStoreFileBatch {
            id: row.get("id"),
            vector_store_id: row.get("vector_store_id"),
            workspace_id: row.get("workspace_id"),
            status: row.get("status"),
            file_counts_in_progress: row.get("file_counts_in_progress"),
            file_counts_completed: row.get("file_counts_completed"),
            file_counts_failed: row.get("file_counts_failed"),
            file_counts_cancelled: row.get("file_counts_cancelled"),
            file_counts_total: row.get("file_counts_total"),
            attributes: row.get("attributes"),
            chunking_strategy: row.get("chunking_strategy"),
            created_at: row.get("created_at"),
            completed_at: row.get("completed_at"),
            updated_at: row.get("updated_at"),
        })
    }

    pub async fn create(
        &self,
        params: CreateVectorStoreFileBatchParams,
    ) -> Result<VectorStoreFileBatch, RepositoryError> {
        let batch_id = Uuid::new_v4();
        let attributes = params
            .attributes
            .clone()
            .unwrap_or_else(|| serde_json::json!({}));
        let total_files = params.file_ids.len() as i32;

        // Use a transaction: insert batch + insert file rows
        let mut client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let txn = client.transaction().await.map_err(map_db_error)?;

        let now = Utc::now();

        // Insert batch row — files are completed immediately so batch is also completed
        let batch_row = txn
            .query_one(
                r#"
                INSERT INTO vector_store_file_batches (
                    id, vector_store_id, workspace_id, status,
                    file_counts_completed, file_counts_total,
                    attributes, chunking_strategy,
                    created_at, completed_at, updated_at
                )
                VALUES ($1, $2, $3, 'completed', $4, $4, $5, $6, $7, $7, $7)
                RETURNING *
                "#,
                &[
                    &batch_id,
                    &params.vector_store_id,
                    &params.workspace_id,
                    &total_files,
                    &attributes,
                    &params.chunking_strategy,
                    &now,
                ],
            )
            .await
            .map_err(map_db_error)?;

        // Insert file rows with ON CONFLICT DO NOTHING
        for file_id in &params.file_ids {
            let vsf_id = Uuid::new_v4();
            txn.execute(
                r#"
                INSERT INTO vector_store_files (
                    id, vector_store_id, file_id, workspace_id, batch_id,
                    status, chunking_strategy, attributes,
                    created_at, processing_started_at, processing_completed_at, updated_at
                )
                VALUES ($1, $2, $3, $4, $5, 'completed', $6, $7, $8, $8, $8, $8)
                ON CONFLICT (vector_store_id, file_id) DO NOTHING
                "#,
                &[
                    &vsf_id,
                    &params.vector_store_id,
                    file_id,
                    &params.workspace_id,
                    &batch_id,
                    &params.chunking_strategy,
                    &attributes,
                    &now,
                ],
            )
            .await
            .map_err(map_db_error)?;
        }

        // Recount batch file_counts based on actual inserts
        txn.execute(
            r#"
            UPDATE vector_store_file_batches SET
                file_counts_completed = (SELECT COUNT(*) FROM vector_store_files WHERE batch_id = $1 AND status = 'completed'),
                file_counts_total     = (SELECT COUNT(*) FROM vector_store_files WHERE batch_id = $1)
            WHERE id = $1
            "#,
            &[&batch_id],
        )
        .await
        .map_err(map_db_error)?;

        txn.commit().await.map_err(map_db_error)?;

        // Re-fetch the batch after counts update
        let updated_row = retry_db!("get_batch_after_create", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    "SELECT * FROM vector_store_file_batches WHERE id = $1",
                    &[&batch_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        let row = updated_row.unwrap_or(batch_row);

        self.row_to_model(row)
            .map_err(RepositoryError::DataConversionError)
    }

    pub async fn get(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<Option<VectorStoreFileBatch>, RepositoryError> {
        let row = retry_db!("get_vector_store_file_batch", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    "SELECT * FROM vector_store_file_batches WHERE id = $1 AND vector_store_id = $2 AND workspace_id = $3",
                    &[&id, &vector_store_id, &workspace_id],
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

    pub async fn cancel(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<Option<VectorStoreFileBatch>, RepositoryError> {
        // Only cancel if in_progress — completed/cancelled/failed batches are returned as-is
        let row = retry_db!("cancel_vector_store_file_batch", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            // Try to update status to cancelled (only if in_progress)
            let updated = client
                .query_opt(
                    r#"
                    UPDATE vector_store_file_batches
                    SET status = 'cancelled'
                    WHERE id = $1 AND vector_store_id = $2 AND workspace_id = $3
                      AND status = 'in_progress'
                    RETURNING *
                    "#,
                    &[&id, &vector_store_id, &workspace_id],
                )
                .await
                .map_err(map_db_error)?;

            // If already completed/cancelled, just return the current row
            match updated {
                Some(row) => Ok(Some(row)),
                None => {
                    client
                        .query_opt(
                            "SELECT * FROM vector_store_file_batches WHERE id = $1 AND vector_store_id = $2 AND workspace_id = $3",
                            &[&id, &vector_store_id, &workspace_id],
                        )
                        .await
                        .map_err(map_db_error)
                }
            }
        })?;

        match row {
            Some(r) => Ok(Some(
                self.row_to_model(r)
                    .map_err(RepositoryError::DataConversionError)?,
            )),
            None => Ok(None),
        }
    }
}

#[async_trait]
impl VectorStoreFileBatchRepository for PgVectorStoreFileBatchRepository {
    async fn create(
        &self,
        params: CreateVectorStoreFileBatchParams,
    ) -> Result<VectorStoreFileBatch, RepositoryError> {
        self.create(params).await
    }

    async fn get(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<Option<VectorStoreFileBatch>, RepositoryError> {
        self.get(id, vector_store_id, workspace_id).await
    }

    async fn cancel(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<Option<VectorStoreFileBatch>, RepositoryError> {
        self.cancel(id, vector_store_id, workspace_id).await
    }
}
