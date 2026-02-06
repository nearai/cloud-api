use async_trait::async_trait;

#[cfg(any(test, feature = "test-mocks"))]
use mockall::automock;

/// Error type for RAG service operations
#[derive(Debug, thiserror::Error)]
pub enum RagError {
    #[error("RAG service request failed: {0}")]
    RequestFailed(String),
    #[error("RAG service returned an error: {status} {body}")]
    ApiError { status: u16, body: String },
    #[error("RAG service response parsing failed: {0}")]
    ParseError(String),
    #[error("RAG service not configured")]
    NotConfigured,
}

/// Trait for interacting with the RAG service.
///
/// All methods use `serde_json::Value` passthrough for forward-compatibility.
/// Cloud-api only adds/strips ID prefixes. All other fields flow through untouched.
#[cfg_attr(any(test, feature = "test-mocks"), automock)]
#[async_trait]
pub trait RagServiceTrait: Send + Sync {
    // -----------------------------------------------------------------------
    // Vector Stores
    // -----------------------------------------------------------------------

    async fn create_vector_store(
        &self,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, RagError>;

    async fn get_vector_store(&self, rag_id: &str) -> Result<serde_json::Value, RagError>;

    async fn list_vector_stores(&self, rag_ids: &[String]) -> Result<serde_json::Value, RagError>;

    async fn update_vector_store(
        &self,
        rag_id: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, RagError>;

    async fn delete_vector_store(&self, rag_id: &str) -> Result<serde_json::Value, RagError>;

    async fn search_vector_store(
        &self,
        rag_vs_id: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, RagError>;

    // -----------------------------------------------------------------------
    // Vector Store Files
    // -----------------------------------------------------------------------

    async fn attach_file(
        &self,
        rag_vs_id: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, RagError>;

    async fn get_vs_file(
        &self,
        rag_vs_id: &str,
        rag_file_id: &str,
    ) -> Result<serde_json::Value, RagError>;

    async fn list_vs_files(
        &self,
        rag_vs_id: &str,
        query_string: &str,
    ) -> Result<serde_json::Value, RagError>;

    async fn update_vs_file(
        &self,
        rag_vs_id: &str,
        rag_file_id: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, RagError>;

    async fn detach_file(
        &self,
        rag_vs_id: &str,
        rag_file_id: &str,
    ) -> Result<serde_json::Value, RagError>;

    // -----------------------------------------------------------------------
    // File Batches
    // -----------------------------------------------------------------------

    async fn create_file_batch(
        &self,
        rag_vs_id: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, RagError>;

    async fn get_file_batch(
        &self,
        rag_vs_id: &str,
        rag_batch_id: &str,
    ) -> Result<serde_json::Value, RagError>;

    async fn cancel_file_batch(
        &self,
        rag_vs_id: &str,
        rag_batch_id: &str,
    ) -> Result<serde_json::Value, RagError>;

    async fn list_batch_files(
        &self,
        rag_vs_id: &str,
        rag_batch_id: &str,
        query_string: &str,
    ) -> Result<serde_json::Value, RagError>;
}
