use crate::retry_db;
use crate::{pool::DbPool, repositories::utils::map_db_error};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
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
        params: services::files::ports::CreateFileParams,
    ) -> Result<File, RepositoryError> {
        let id = Uuid::new_v4();

        let row = match retry_db!("create_new_file_record", {
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
                INSERT INTO files (
                    id, filename, bytes, content_type, purpose, storage_key,
                    workspace_id, uploaded_by_api_key_id, created_at, expires_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                RETURNING *
                "#,
                    &[
                        &id,
                        &params.filename,
                        &params.bytes,
                        &params.content_type,
                        &params.purpose,
                        &params.storage_key,
                        &params.workspace_id,
                        &params.uploaded_by_api_key_id,
                        &now,
                        &params.expires_at,
                    ],
                )
                .await
                .map_err(map_db_error)
        }) {
            Ok(row) => row,
            Err(RepositoryError::AlreadyExists) => {
                // INSERT succeeded but connection dropped before response
                // Record exists, fetch and return it (idempotent retry)
                debug!(
                    "File {} already exists, fetching existing record (idempotent retry)",
                    id
                );
                retry_db!("get_file_by_id_after_conflict", {
                    let client = self
                        .pool
                        .get()
                        .await
                        .context("Failed to get database connection")
                        .map_err(RepositoryError::PoolError)?;

                    client
                        .query_opt("SELECT * FROM files WHERE id = $1", &[&id])
                        .await
                        .map_err(map_db_error)
                })?
                .ok_or_else(|| {
                    RepositoryError::DatabaseError(anyhow::anyhow!(
                        "File {id} was reported as existing but not found"
                    ))
                })?
            }
            Err(e) => return Err(e),
        };

        debug!(
            "Created file: {} ({} bytes) for workspace: {}",
            id, params.bytes, params.workspace_id
        );

        self.row_to_file(row)
            .map_err(RepositoryError::DataConversionError)
    }

    /// Get a file by ID
    pub async fn get_by_id(&self, id: Uuid) -> Result<Option<File>, RepositoryError> {
        let row = retry_db!("get_file_by_id", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt("SELECT * FROM files WHERE id = $1", &[&id])
                .await
                .map_err(map_db_error)
        })?;

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
        let row = retry_db!("get_file_by_id_and_workspace", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    "SELECT * FROM files WHERE id = $1 AND workspace_id = $2",
                    &[&id, &workspace_id],
                )
                .await
                .map_err(map_db_error)
        })?;

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
        let limit = limit.unwrap_or(100);
        let offset = offset.unwrap_or(0);

        let rows = retry_db!("list_files_by_workspace", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
            .query(
                "SELECT * FROM files WHERE workspace_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
                &[&workspace_id, &limit, &offset],
            )
            .await
            .map_err(map_db_error)
        })?;

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
        let limit = limit.unwrap_or(100);
        let offset = offset.unwrap_or(0);

        let rows = retry_db!("list_files_by_workspace_and_purpose", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
            .query(
                "SELECT * FROM files WHERE workspace_id = $1 AND purpose = $2 ORDER BY created_at DESC LIMIT $3 OFFSET $4",
                &[&workspace_id, &purpose, &limit, &offset],
            )
            .await
            .map_err(map_db_error)
        })?;

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
        let order_clause = if order == "asc" { "ASC" } else { "DESC" };
        let comparison = if order == "asc" { ">" } else { "<" };

        let query = match (after, purpose.as_ref()) {
            // With cursor and purpose filter
            (Some(_), Some(_)) => format!(
                "SELECT f.* FROM files f
                     WHERE f.workspace_id = $1
                     AND f.purpose = $2
                     AND f.created_at {comparison} (SELECT created_at FROM files WHERE id = $3)
                     ORDER BY f.created_at {order_clause}
                     LIMIT $4"
            ),
            // With cursor, no purpose filter
            (Some(_), None) => format!(
                "SELECT f.* FROM files f
                     WHERE f.workspace_id = $1
                     AND f.created_at {comparison} (SELECT created_at FROM files WHERE id = $2)
                     ORDER BY f.created_at {order_clause}
                     LIMIT $3"
            ),
            // No cursor, with purpose filter
            (None, Some(_)) => format!(
                "SELECT * FROM files
                     WHERE workspace_id = $1
                     AND purpose = $2
                     ORDER BY created_at {order_clause}
                     LIMIT $3"
            ),
            // No cursor, no purpose filter
            (None, None) => format!(
                "SELECT * FROM files
                     WHERE workspace_id = $1
                     ORDER BY created_at {order_clause}
                     LIMIT $2"
            ),
        };

        let rows = retry_db!("list_files_with_cursor_pagination", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            match (after, purpose.as_ref()) {
                (Some(after_id), Some(purpose_str)) => {
                    client
                        .query(&query, &[&workspace_id, purpose_str, &after_id, &limit])
                        .await
                }
                (Some(after_id), None) => {
                    client
                        .query(&query, &[&workspace_id, &after_id, &limit])
                        .await
                }
                (None, Some(purpose_str)) => {
                    client
                        .query(&query, &[&workspace_id, purpose_str, &limit])
                        .await
                }
                (None, None) => client.query(&query, &[&workspace_id, &limit]).await,
            }
            .map_err(map_db_error)
        })?;

        rows.into_iter()
            .map(|row| {
                self.row_to_file(row)
                    .map_err(RepositoryError::DataConversionError)
            })
            .collect()
    }

    /// Delete a file record
    pub async fn delete(&self, id: Uuid) -> Result<bool, RepositoryError> {
        let rows_affected = retry_db!("delete_file_record", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute("DELETE FROM files WHERE id = $1", &[&id])
                .await
                .map_err(map_db_error)
        })?;

        Ok(rows_affected > 0)
    }

    /// Get expired files
    pub async fn get_expired_files(&self) -> Result<Vec<File>, RepositoryError> {
        let rows = retry_db!("get_expired_files", {
            let now = Utc::now();
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    "SELECT * FROM files WHERE expires_at IS NOT NULL AND expires_at < $1",
                    &[&now],
                )
                .await
                .map_err(map_db_error)
        })?;

        rows.into_iter()
            .map(|row| {
                self.row_to_file(row)
                    .map_err(RepositoryError::DataConversionError)
            })
            .collect()
    }

    /// Fetch multiple files by IDs within a workspace
    pub async fn get_by_ids_and_workspace(
        &self,
        ids: &[Uuid],
        workspace_id: Uuid,
    ) -> Result<Vec<File>, RepositoryError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }

        let rows = retry_db!("get_files_by_ids_and_workspace", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    "SELECT * FROM files WHERE id = ANY($1) AND workspace_id = $2",
                    &[&ids, &workspace_id],
                )
                .await
                .map_err(map_db_error)
        })?;

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
            uploaded_by_api_key_id: row.get("uploaded_by_api_key_id"),
            created_at: row.get("created_at"),
            expires_at: row.get("expires_at"),
        })
    }
}

#[async_trait]
impl FileRepositoryTrait for FileRepository {
    async fn create(
        &self,
        params: services::files::ports::CreateFileParams,
    ) -> Result<File, RepositoryError> {
        self.create(params).await
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

    async fn get_by_ids_and_workspace(
        &self,
        ids: &[Uuid],
        workspace_id: Uuid,
    ) -> Result<Vec<File>, RepositoryError> {
        self.get_by_ids_and_workspace(ids, workspace_id).await
    }
}
