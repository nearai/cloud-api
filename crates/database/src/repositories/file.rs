use crate::{pool::DbPool, repositories::utils::map_db_error};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use services::common::RepositoryError;
use services::files::{File, FileRepositoryTrait};
use tracing::debug;
use uuid::Uuid;

pub struct FileRepository {
    pool: DbPool,
}

impl FileRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Create a new file record
    pub async fn create(
        &self,
        filename: String,
        bytes: i64,
        content_type: String,
        purpose: String,
        storage_key: String,
        workspace_id: Uuid,
        uploaded_by_user_id: Option<Uuid>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<File, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let id = Uuid::new_v4();
        let now = Utc::now();

        let row = client
            .query_one(
                r#"
                INSERT INTO files (
                    id, filename, bytes, content_type, purpose, storage_key,
                    workspace_id, uploaded_by_user_id, created_at, expires_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                RETURNING *
                "#,
                &[
                    &id,
                    &filename,
                    &bytes,
                    &content_type,
                    &purpose,
                    &storage_key,
                    &workspace_id,
                    &uploaded_by_user_id,
                    &now,
                    &expires_at,
                ],
            )
            .await
            .map_err(map_db_error)?;

        debug!(
            "Created file: {} ({} bytes) for workspace: {}",
            id, bytes, workspace_id
        );

        self.row_to_file(row)
            .map_err(RepositoryError::DataConversionError)
    }

    /// Get a file by ID
    pub async fn get_by_id(&self, id: Uuid) -> Result<Option<File>, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let row = client
            .query_opt("SELECT * FROM files WHERE id = $1", &[&id])
            .await
            .map_err(map_db_error)?;

        match row {
            Some(row) => Ok(Some(
                self.row_to_file(row)
                    .map_err(RepositoryError::DataConversionError)?,
            )),
            None => Ok(None),
        }
    }

    /// Get a file by ID within a workspace (for authorization)
    pub async fn get_by_id_and_workspace(
        &self,
        id: Uuid,
        workspace_id: Uuid,
    ) -> Result<Option<File>, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let row = client
            .query_opt(
                "SELECT * FROM files WHERE id = $1 AND workspace_id = $2",
                &[&id, &workspace_id],
            )
            .await
            .map_err(map_db_error)?;

        match row {
            Some(row) => Ok(Some(
                self.row_to_file(row)
                    .map_err(RepositoryError::DataConversionError)?,
            )),
            None => Ok(None),
        }
    }

    /// List files for a workspace
    pub async fn list_by_workspace(
        &self,
        workspace_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<File>, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let limit = limit.unwrap_or(100);
        let offset = offset.unwrap_or(0);

        let rows = client
            .query(
                "SELECT * FROM files WHERE workspace_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
                &[&workspace_id, &limit, &offset],
            )
            .await
            .map_err(map_db_error)?;

        rows.into_iter()
            .map(|row| {
                self.row_to_file(row)
                    .map_err(RepositoryError::DataConversionError)
            })
            .collect()
    }

    /// List files by purpose for a workspace
    pub async fn list_by_workspace_and_purpose(
        &self,
        workspace_id: Uuid,
        purpose: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<File>, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let limit = limit.unwrap_or(100);
        let offset = offset.unwrap_or(0);

        let rows = client
            .query(
                "SELECT * FROM files WHERE workspace_id = $1 AND purpose = $2 ORDER BY created_at DESC LIMIT $3 OFFSET $4",
                &[&workspace_id, &purpose, &limit, &offset],
            )
            .await
            .map_err(map_db_error)?;

        rows.into_iter()
            .map(|row| {
                self.row_to_file(row)
                    .map_err(RepositoryError::DataConversionError)
            })
            .collect()
    }

    /// List files with cursor-based pagination
    pub async fn list_with_pagination(
        &self,
        workspace_id: Uuid,
        after: Option<Uuid>,
        limit: i64,
        order: &str,
        purpose: Option<String>,
    ) -> Result<Vec<File>, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let order_clause = if order == "asc" { "ASC" } else { "DESC" };
        let comparison = if order == "asc" { ">" } else { "<" };

        let rows = match (after, purpose) {
            (Some(after_id), Some(purpose_str)) => {
                // With cursor and purpose filter
                let query = format!(
                    "SELECT f.* FROM files f
                     WHERE f.workspace_id = $1
                     AND f.purpose = $2
                     AND f.created_at {} (SELECT created_at FROM files WHERE id = $3)
                     ORDER BY f.created_at {}
                     LIMIT $4",
                    comparison, order_clause
                );
                client
                    .query(&query, &[&workspace_id, &purpose_str, &after_id, &limit])
                    .await
            }
            (Some(after_id), None) => {
                // With cursor, no purpose filter
                let query = format!(
                    "SELECT f.* FROM files f
                     WHERE f.workspace_id = $1
                     AND f.created_at {} (SELECT created_at FROM files WHERE id = $2)
                     ORDER BY f.created_at {}
                     LIMIT $3",
                    comparison, order_clause
                );
                client
                    .query(&query, &[&workspace_id, &after_id, &limit])
                    .await
            }
            (None, Some(purpose_str)) => {
                // No cursor, with purpose filter
                let query = format!(
                    "SELECT * FROM files
                     WHERE workspace_id = $1
                     AND purpose = $2
                     ORDER BY created_at {}
                     LIMIT $3",
                    order_clause
                );
                client
                    .query(&query, &[&workspace_id, &purpose_str, &limit])
                    .await
            }
            (None, None) => {
                // No cursor, no purpose filter
                let query = format!(
                    "SELECT * FROM files
                     WHERE workspace_id = $1
                     ORDER BY created_at {}
                     LIMIT $2",
                    order_clause
                );
                client.query(&query, &[&workspace_id, &limit]).await
            }
        }
        .map_err(map_db_error)?;

        rows.into_iter()
            .map(|row| {
                self.row_to_file(row)
                    .map_err(RepositoryError::DataConversionError)
            })
            .collect()
    }

    /// Delete a file record
    pub async fn delete(&self, id: Uuid) -> Result<bool, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let rows_affected = client
            .execute("DELETE FROM files WHERE id = $1", &[&id])
            .await
            .map_err(map_db_error)?;

        Ok(rows_affected > 0)
    }

    /// Get expired files
    pub async fn get_expired_files(&self) -> Result<Vec<File>, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let now = Utc::now();

        let rows = client
            .query(
                "SELECT * FROM files WHERE expires_at IS NOT NULL AND expires_at < $1",
                &[&now],
            )
            .await
            .map_err(map_db_error)?;

        rows.into_iter()
            .map(|row| {
                self.row_to_file(row)
                    .map_err(RepositoryError::DataConversionError)
            })
            .collect()
    }

    /// Helper function to convert database row to File
    fn row_to_file(&self, row: tokio_postgres::Row) -> Result<File> {
        Ok(File {
            id: row.get("id"),
            filename: row.get("filename"),
            bytes: row.get("bytes"),
            content_type: row.get("content_type"),
            purpose: row.get("purpose"),
            storage_key: row.get("storage_key"),
            workspace_id: row.get("workspace_id"),
            uploaded_by_user_id: row.get("uploaded_by_user_id"),
            created_at: row.get("created_at"),
            expires_at: row.get("expires_at"),
        })
    }
}

#[async_trait]
impl FileRepositoryTrait for FileRepository {
    async fn create(
        &self,
        filename: String,
        bytes: i64,
        content_type: String,
        purpose: String,
        storage_key: String,
        workspace_id: Uuid,
        uploaded_by_user_id: Option<Uuid>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<File, RepositoryError> {
        self.create(
            filename,
            bytes,
            content_type,
            purpose,
            storage_key,
            workspace_id,
            uploaded_by_user_id,
            expires_at,
        )
        .await
    }

    async fn get_by_id(&self, id: Uuid) -> Result<Option<File>, RepositoryError> {
        self.get_by_id(id).await
    }

    async fn get_by_id_and_workspace(
        &self,
        id: Uuid,
        workspace_id: Uuid,
    ) -> Result<Option<File>, RepositoryError> {
        self.get_by_id_and_workspace(id, workspace_id).await
    }

    async fn list_by_workspace(
        &self,
        workspace_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<File>, RepositoryError> {
        self.list_by_workspace(workspace_id, limit, offset).await
    }

    async fn list_by_workspace_and_purpose(
        &self,
        workspace_id: Uuid,
        purpose: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<File>, RepositoryError> {
        self.list_by_workspace_and_purpose(workspace_id, purpose, limit, offset)
            .await
    }

    async fn list_with_pagination(
        &self,
        workspace_id: Uuid,
        after: Option<Uuid>,
        limit: i64,
        order: &str,
        purpose: Option<String>,
    ) -> Result<Vec<File>, RepositoryError> {
        self.list_with_pagination(workspace_id, after, limit, order, purpose)
            .await
    }

    async fn delete(&self, id: Uuid) -> Result<bool, RepositoryError> {
        self.delete(id).await
    }

    async fn get_expired_files(&self) -> Result<Vec<File>, RepositoryError> {
        self.get_expired_files().await
    }
}
