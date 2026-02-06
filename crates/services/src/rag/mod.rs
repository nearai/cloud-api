pub mod client;
pub mod ports;

pub use client::RagServiceClient;
pub use ports::{RagError, RagServiceTrait};

#[cfg(any(test, feature = "test-mocks"))]
pub use ports::MockRagServiceTrait;

/// Stub implementation that returns `NotConfigured` for all methods.
/// Used when the RAG service URL is not set in configuration.
pub struct NotConfiguredRagService;

#[async_trait::async_trait]
impl RagServiceTrait for NotConfiguredRagService {
    async fn create_vector_store(
        &self,
        _body: serde_json::Value,
    ) -> Result<serde_json::Value, RagError> {
        Err(RagError::NotConfigured)
    }
    async fn get_vector_store(&self, _rag_id: &str) -> Result<serde_json::Value, RagError> {
        Err(RagError::NotConfigured)
    }
    async fn list_vector_stores(&self, _rag_ids: &[String]) -> Result<serde_json::Value, RagError> {
        Err(RagError::NotConfigured)
    }
    async fn update_vector_store(
        &self,
        _rag_id: &str,
        _body: serde_json::Value,
    ) -> Result<serde_json::Value, RagError> {
        Err(RagError::NotConfigured)
    }
    async fn delete_vector_store(&self, _rag_id: &str) -> Result<serde_json::Value, RagError> {
        Err(RagError::NotConfigured)
    }
    async fn search_vector_store(
        &self,
        _rag_vs_id: &str,
        _body: serde_json::Value,
    ) -> Result<serde_json::Value, RagError> {
        Err(RagError::NotConfigured)
    }
    async fn attach_file(
        &self,
        _rag_vs_id: &str,
        _body: serde_json::Value,
    ) -> Result<serde_json::Value, RagError> {
        Err(RagError::NotConfigured)
    }
    async fn get_vs_file(
        &self,
        _rag_vs_id: &str,
        _rag_file_id: &str,
    ) -> Result<serde_json::Value, RagError> {
        Err(RagError::NotConfigured)
    }
    async fn list_vs_files(
        &self,
        _rag_vs_id: &str,
        _query_string: &str,
    ) -> Result<serde_json::Value, RagError> {
        Err(RagError::NotConfigured)
    }
    async fn update_vs_file(
        &self,
        _rag_vs_id: &str,
        _rag_file_id: &str,
        _body: serde_json::Value,
    ) -> Result<serde_json::Value, RagError> {
        Err(RagError::NotConfigured)
    }
    async fn detach_file(
        &self,
        _rag_vs_id: &str,
        _rag_file_id: &str,
    ) -> Result<serde_json::Value, RagError> {
        Err(RagError::NotConfigured)
    }
    async fn create_file_batch(
        &self,
        _rag_vs_id: &str,
        _body: serde_json::Value,
    ) -> Result<serde_json::Value, RagError> {
        Err(RagError::NotConfigured)
    }
    async fn get_file_batch(
        &self,
        _rag_vs_id: &str,
        _rag_batch_id: &str,
    ) -> Result<serde_json::Value, RagError> {
        Err(RagError::NotConfigured)
    }
    async fn cancel_file_batch(
        &self,
        _rag_vs_id: &str,
        _rag_batch_id: &str,
    ) -> Result<serde_json::Value, RagError> {
        Err(RagError::NotConfigured)
    }
    async fn list_batch_files(
        &self,
        _rag_vs_id: &str,
        _rag_batch_id: &str,
        _query_string: &str,
    ) -> Result<serde_json::Value, RagError> {
        Err(RagError::NotConfigured)
    }
}
