use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::common::RepositoryError;

// ---------------------------------------------------------------------------
// Domain Model — Thin local ref (auth + pagination only)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct VectorStoreRef {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Pagination Params
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PaginationParams {
    pub limit: u32,
    pub order: String, // "asc" or "desc"
    pub after: Option<Uuid>,
    pub before: Option<Uuid>,
}

// ---------------------------------------------------------------------------
// Repository Trait — Thin ref table only
// ---------------------------------------------------------------------------

#[async_trait]
pub trait VectorStoreRefRepository: Send + Sync {
    async fn create(&self, id: Uuid, workspace_id: Uuid)
        -> Result<VectorStoreRef, RepositoryError>;

    async fn get(
        &self,
        id: Uuid,
        workspace_id: Uuid,
    ) -> Result<Option<VectorStoreRef>, RepositoryError>;

    async fn list(
        &self,
        workspace_id: Uuid,
        params: &PaginationParams,
    ) -> Result<(Vec<VectorStoreRef>, bool), RepositoryError>;

    async fn soft_delete(&self, id: Uuid, workspace_id: Uuid) -> Result<bool, RepositoryError>;
}
