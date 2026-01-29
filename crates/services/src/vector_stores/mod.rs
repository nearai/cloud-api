pub mod ports;

pub use ports::{
    CreateVectorStoreFileBatchParams, CreateVectorStoreFileParams, CreateVectorStoreParams,
    ListParams, UpdateVectorStoreParams, VectorStore, VectorStoreFile, VectorStoreFileBatch,
    VectorStoreFileBatchRepository, VectorStoreFileRepository, VectorStoreRepository,
};

use crate::common::RepositoryError;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Service Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum VectorStoreServiceError {
    #[error("Vector store not found")]
    NotFound,
    #[error("Vector store file not found")]
    FileNotFound,
    #[error("File batch not found")]
    BatchNotFound,
    #[error("Invalid parameters: {0}")]
    InvalidParams(String),
    #[error("File already exists in vector store")]
    FileAlreadyExists,
    #[error("Repository error: {0}")]
    RepositoryError(#[from] RepositoryError),
}

// ---------------------------------------------------------------------------
// Service Trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait VectorStoreServiceTrait: Send + Sync {
    async fn create_vector_store(
        &self,
        params: CreateVectorStoreParams,
    ) -> Result<VectorStore, VectorStoreServiceError>;

    async fn get_vector_store(
        &self,
        id: Uuid,
        workspace_id: Uuid,
    ) -> Result<VectorStore, VectorStoreServiceError>;

    async fn list_vector_stores(
        &self,
        params: &ListParams,
    ) -> Result<Vec<VectorStore>, VectorStoreServiceError>;

    async fn update_vector_store(
        &self,
        id: Uuid,
        workspace_id: Uuid,
        params: &UpdateVectorStoreParams,
    ) -> Result<VectorStore, VectorStoreServiceError>;

    async fn delete_vector_store(
        &self,
        id: Uuid,
        workspace_id: Uuid,
    ) -> Result<bool, VectorStoreServiceError>;

    async fn create_vector_store_file(
        &self,
        params: CreateVectorStoreFileParams,
    ) -> Result<VectorStoreFile, VectorStoreServiceError>;

    async fn get_vector_store_file(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<VectorStoreFile, VectorStoreServiceError>;

    async fn list_vector_store_files(
        &self,
        vector_store_id: Uuid,
        workspace_id: Uuid,
        params: &ListParams,
    ) -> Result<Vec<VectorStoreFile>, VectorStoreServiceError>;

    async fn update_vector_store_file_attributes(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
        attributes: Value,
    ) -> Result<VectorStoreFile, VectorStoreServiceError>;

    async fn delete_vector_store_file(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<bool, VectorStoreServiceError>;

    async fn create_file_batch(
        &self,
        params: CreateVectorStoreFileBatchParams,
    ) -> Result<VectorStoreFileBatch, VectorStoreServiceError>;

    async fn get_file_batch(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<VectorStoreFileBatch, VectorStoreServiceError>;

    async fn cancel_file_batch(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<VectorStoreFileBatch, VectorStoreServiceError>;

    async fn list_file_batch_files(
        &self,
        batch_id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
        params: &ListParams,
    ) -> Result<Vec<VectorStoreFile>, VectorStoreServiceError>;
}

// ---------------------------------------------------------------------------
// Service Implementation
// ---------------------------------------------------------------------------

pub struct VectorStoreServiceImpl {
    store_repo: Arc<dyn VectorStoreRepository>,
    file_repo: Arc<dyn VectorStoreFileRepository>,
    batch_repo: Arc<dyn VectorStoreFileBatchRepository>,
}

impl VectorStoreServiceImpl {
    pub fn new(
        store_repo: Arc<dyn VectorStoreRepository>,
        file_repo: Arc<dyn VectorStoreFileRepository>,
        batch_repo: Arc<dyn VectorStoreFileBatchRepository>,
    ) -> Self {
        Self {
            store_repo,
            file_repo,
            batch_repo,
        }
    }

    /// Verify vector store exists and belongs to workspace.
    async fn verify_store_ownership(
        &self,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<VectorStore, VectorStoreServiceError> {
        self.store_repo
            .get_by_id_and_workspace(vector_store_id, workspace_id)
            .await?
            .ok_or(VectorStoreServiceError::NotFound)
    }
}

#[async_trait]
impl VectorStoreServiceTrait for VectorStoreServiceImpl {
    async fn create_vector_store(
        &self,
        params: CreateVectorStoreParams,
    ) -> Result<VectorStore, VectorStoreServiceError> {
        let vs = self.store_repo.create(params).await?;
        Ok(vs)
    }

    async fn get_vector_store(
        &self,
        id: Uuid,
        workspace_id: Uuid,
    ) -> Result<VectorStore, VectorStoreServiceError> {
        self.verify_store_ownership(id, workspace_id).await
    }

    async fn list_vector_stores(
        &self,
        params: &ListParams,
    ) -> Result<Vec<VectorStore>, VectorStoreServiceError> {
        let stores = self.store_repo.list(params).await?;
        Ok(stores)
    }

    async fn update_vector_store(
        &self,
        id: Uuid,
        workspace_id: Uuid,
        params: &UpdateVectorStoreParams,
    ) -> Result<VectorStore, VectorStoreServiceError> {
        let vs = self
            .store_repo
            .update(id, workspace_id, params)
            .await?
            .ok_or(VectorStoreServiceError::NotFound)?;
        Ok(vs)
    }

    async fn delete_vector_store(
        &self,
        id: Uuid,
        workspace_id: Uuid,
    ) -> Result<bool, VectorStoreServiceError> {
        let deleted = self.store_repo.soft_delete(id, workspace_id).await?;
        if !deleted {
            return Err(VectorStoreServiceError::NotFound);
        }
        Ok(true)
    }

    async fn create_vector_store_file(
        &self,
        params: CreateVectorStoreFileParams,
    ) -> Result<VectorStoreFile, VectorStoreServiceError> {
        // Verify ownership
        self.verify_store_ownership(params.vector_store_id, params.workspace_id)
            .await?;

        let vsf = match self.file_repo.create(params.clone()).await {
            Ok(f) => f,
            Err(RepositoryError::AlreadyExists) => {
                return Err(VectorStoreServiceError::FileAlreadyExists);
            }
            Err(e) => return Err(e.into()),
        };

        // Recalculate file counts on the parent vector store
        let _ = self
            .store_repo
            .update_file_counts(params.vector_store_id)
            .await;

        Ok(vsf)
    }

    async fn get_vector_store_file(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<VectorStoreFile, VectorStoreServiceError> {
        self.verify_store_ownership(vector_store_id, workspace_id)
            .await?;

        self.file_repo
            .get(id, vector_store_id, workspace_id)
            .await?
            .ok_or(VectorStoreServiceError::FileNotFound)
    }

    async fn list_vector_store_files(
        &self,
        vector_store_id: Uuid,
        workspace_id: Uuid,
        params: &ListParams,
    ) -> Result<Vec<VectorStoreFile>, VectorStoreServiceError> {
        self.verify_store_ownership(vector_store_id, workspace_id)
            .await?;

        let files = self
            .file_repo
            .list(vector_store_id, workspace_id, params)
            .await?;
        Ok(files)
    }

    async fn update_vector_store_file_attributes(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
        attributes: Value,
    ) -> Result<VectorStoreFile, VectorStoreServiceError> {
        self.verify_store_ownership(vector_store_id, workspace_id)
            .await?;

        self.file_repo
            .update_attributes(id, vector_store_id, workspace_id, attributes)
            .await?
            .ok_or(VectorStoreServiceError::FileNotFound)
    }

    async fn delete_vector_store_file(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<bool, VectorStoreServiceError> {
        self.verify_store_ownership(vector_store_id, workspace_id)
            .await?;

        let deleted = self
            .file_repo
            .delete(id, vector_store_id, workspace_id)
            .await?;
        if !deleted {
            return Err(VectorStoreServiceError::FileNotFound);
        }

        // Recalculate file counts
        let _ = self.store_repo.update_file_counts(vector_store_id).await;

        Ok(true)
    }

    async fn create_file_batch(
        &self,
        params: CreateVectorStoreFileBatchParams,
    ) -> Result<VectorStoreFileBatch, VectorStoreServiceError> {
        self.verify_store_ownership(params.vector_store_id, params.workspace_id)
            .await?;

        if params.file_ids.is_empty() {
            return Err(VectorStoreServiceError::InvalidParams(
                "file_ids must not be empty".to_string(),
            ));
        }

        let batch = self.batch_repo.create(params.clone()).await?;

        // Recalculate file counts on the parent vector store
        let _ = self
            .store_repo
            .update_file_counts(params.vector_store_id)
            .await;

        Ok(batch)
    }

    async fn get_file_batch(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<VectorStoreFileBatch, VectorStoreServiceError> {
        self.verify_store_ownership(vector_store_id, workspace_id)
            .await?;

        self.batch_repo
            .get(id, vector_store_id, workspace_id)
            .await?
            .ok_or(VectorStoreServiceError::BatchNotFound)
    }

    async fn cancel_file_batch(
        &self,
        id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<VectorStoreFileBatch, VectorStoreServiceError> {
        self.verify_store_ownership(vector_store_id, workspace_id)
            .await?;

        self.batch_repo
            .cancel(id, vector_store_id, workspace_id)
            .await?
            .ok_or(VectorStoreServiceError::BatchNotFound)
    }

    async fn list_file_batch_files(
        &self,
        batch_id: Uuid,
        vector_store_id: Uuid,
        workspace_id: Uuid,
        params: &ListParams,
    ) -> Result<Vec<VectorStoreFile>, VectorStoreServiceError> {
        self.verify_store_ownership(vector_store_id, workspace_id)
            .await?;

        // Verify batch exists
        self.batch_repo
            .get(batch_id, vector_store_id, workspace_id)
            .await?
            .ok_or(VectorStoreServiceError::BatchNotFound)?;

        let files = self
            .file_repo
            .list_by_batch(batch_id, vector_store_id, workspace_id, params)
            .await?;
        Ok(files)
    }
}
