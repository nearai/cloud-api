use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::common::RepositoryError;

// ---------------------------------------------------------------------------
// Domain Models
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorStore {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: Option<String>,
    pub description: Option<String>,
    pub status: String,
    pub usage_bytes: i64,
    pub file_counts_in_progress: i32,
    pub file_counts_completed: i32,
    pub file_counts_failed: i32,
    pub file_counts_cancelled: i32,
    pub file_counts_total: i32,
    pub last_active_at: DateTime<Utc>,
    pub expires_after_anchor: Option<String>,
    pub expires_after_days: Option<i32>,
    pub expires_at: Option<DateTime<Utc>>,
    pub metadata: Value,
    pub chunking_strategy: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorStoreFile {
    pub id: Uuid,
    pub vector_store_id: Uuid,
    pub file_id: Uuid,
    pub workspace_id: Uuid,
    pub batch_id: Option<Uuid>,
    pub status: String,
    pub usage_bytes: i64,
    pub chunk_count: i32,
    pub chunking_strategy: Option<Value>,
    pub attributes: Value,
    pub last_error: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub processing_started_at: Option<DateTime<Utc>>,
    pub processing_completed_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorStoreFileBatch {
    pub id: Uuid,
    pub vector_store_id: Uuid,
    pub workspace_id: Uuid,
    pub status: String,
    pub file_counts_in_progress: i32,
    pub file_counts_completed: i32,
    pub file_counts_failed: i32,
    pub file_counts_cancelled: i32,
    pub file_counts_total: i32,
    pub attributes: Value,
    pub chunking_strategy: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Param Structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CreateVectorStoreParams {
    pub workspace_id: Uuid,
    pub name: Option<String>,
    pub description: Option<String>,
    pub expires_after_anchor: Option<String>,
    pub expires_after_days: Option<i32>,
    pub metadata: Option<Value>,
    pub chunking_strategy: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct UpdateVectorStoreParams {
    pub name: Option<String>,
    pub expires_after_anchor: Option<String>,
    pub expires_after_days: Option<i32>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct CreateVectorStoreFileParams {
    pub vector_store_id: Uuid,
    pub file_id: Uuid,
    pub workspace_id: Uuid,
    pub batch_id: Option<Uuid>,
    pub chunking_strategy: Option<Value>,
    pub attributes: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct ListParams {
    pub workspace_id: Uuid,
    pub limit: i64,
    pub order: String,
    pub after: Option<Uuid>,
    pub before: Option<Uuid>,
    pub filter: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CreateVectorStoreFileBatchParams {
    pub vector_store_id: Uuid,
    pub workspace_id: Uuid,
    pub file_ids: Vec<Uuid>,
    pub chunking_strategy: Option<Value>,
    pub attributes: Option<Value>,
}

// ---------------------------------------------------------------------------
// Repository Traits
// ---------------------------------------------------------------------------

#[async_trait]
pub trait VectorStoreRepository: Send + Sync {
    async fn create(&self, params: CreateVectorStoreParams)
        -> Result<VectorStore, RepositoryError>;

    async fn get_by_id_and_workspace(
        &self,
        id: Uuid,
        workspace_id: Uuid,
    ) -> Result<Option<VectorStore>, RepositoryError>;

    async fn list(&self, params: &ListParams) -> Result<Vec<VectorStore>, RepositoryError>;

    async fn update(
        &self,
        id: Uuid,
        workspace_id: Uuid,
        params: &UpdateVectorStoreParams,
    ) -> Result<Option<VectorStore>, RepositoryError>;

    async fn soft_delete(&self, id: Uuid, workspace_id: Uuid) -> Result<bool, RepositoryError>;

    async fn update_file_counts(&self, id: Uuid) -> Result<(), RepositoryError>;
}

#[async_trait]
pub trait VectorStoreFileRepository: Send + Sync {
    async fn create(
        &self,
        params: CreateVectorStoreFileParams,
    ) -> Result<VectorStoreFile, RepositoryError>;

    async fn get(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<Option<VectorStoreFile>, RepositoryError>;

    async fn list(
        &self,
        vector_store_id: Uuid,
        workspace_id: Uuid,
        params: &ListParams,
    ) -> Result<Vec<VectorStoreFile>, RepositoryError>;

    async fn update_attributes(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
        attributes: Value,
    ) -> Result<Option<VectorStoreFile>, RepositoryError>;

    async fn delete(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<bool, RepositoryError>;

    async fn list_by_batch(
        &self,
        batch_id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
        params: &ListParams,
    ) -> Result<Vec<VectorStoreFile>, RepositoryError>;
}

#[async_trait]
pub trait VectorStoreFileBatchRepository: Send + Sync {
    async fn create(
        &self,
        params: CreateVectorStoreFileBatchParams,
    ) -> Result<VectorStoreFileBatch, RepositoryError>;

    async fn get(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<Option<VectorStoreFileBatch>, RepositoryError>;

    async fn cancel(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<Option<VectorStoreFileBatch>, RepositoryError>;
}
