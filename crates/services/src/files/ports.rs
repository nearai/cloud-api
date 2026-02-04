use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::RepositoryError;

/// Domain model for file metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct File {
    pub id: Uuid,
    pub filename: String,
    pub bytes: i64,
    pub content_type: String,
    pub purpose: String,
    pub storage_key: String,
    pub workspace_id: Uuid,
    pub uploaded_by_api_key_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Parameters for creating a file record
#[derive(Debug, Clone)]
pub struct CreateFileParams {
    pub filename: String,
    pub bytes: i64,
    pub content_type: String,
    pub purpose: String,
    pub storage_key: String,
    pub workspace_id: Uuid,
    pub uploaded_by_api_key_id: Uuid,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Repository trait for file operations
#[async_trait]
pub trait FileRepositoryTrait: Send + Sync {
    async fn create(&self, params: CreateFileParams) -> Result<File, RepositoryError>;

    async fn get_by_id(&self, id: Uuid) -> Result<Option<File>, RepositoryError>;

    async fn get_by_id_and_workspace(
        &self,
        id: Uuid,
        workspace_id: Uuid,
    ) -> Result<Option<File>, RepositoryError>;

    async fn list_by_workspace(
        &self,
        workspace_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<File>, RepositoryError>;

    async fn list_by_workspace_and_purpose(
        &self,
        workspace_id: Uuid,
        purpose: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<File>, RepositoryError>;

    async fn list_with_pagination(
        &self,
        workspace_id: Uuid,
        after: Option<Uuid>,
        limit: i64,
        order: &str,
        purpose: Option<String>,
    ) -> Result<Vec<File>, RepositoryError>;

    async fn delete(&self, id: Uuid) -> Result<bool, RepositoryError>;

    async fn get_expired_files(&self) -> Result<Vec<File>, RepositoryError>;

    /// Verify that ALL given file IDs belong to the specified workspace.
    /// Returns true if all files exist and belong to the workspace, false otherwise.
    async fn verify_workspace_ownership(
        &self,
        file_ids: &[Uuid],
        workspace_id: Uuid,
    ) -> Result<bool, RepositoryError>;
}
