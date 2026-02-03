use async_trait::async_trait;
use serde::{Deserialize, Serialize};

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

/// Vector store representation from RAG service
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorStore {
    pub id: String,
    pub name: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub created_at: i64,
}

/// File representation from RAG service
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagFile {
    pub id: String,
    pub filename: String,
    pub bytes: u64,
    pub purpose: String,
    pub status: String,
    pub created_at: i64,
}

/// Search result from RAG service
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub file_id: String,
    pub file_name: String,
    pub content: String,
    pub score: f32,
}

/// Trait for interacting with the RAG service
#[async_trait]
pub trait RagServiceTrait: Send + Sync {
    // Vector store operations
    async fn create_vector_store(
        &self,
        name: &str,
        metadata: Option<serde_json::Value>,
    ) -> Result<VectorStore, RagError>;

    async fn get_vector_store(&self, id: &str) -> Result<VectorStore, RagError>;

    async fn delete_vector_store(&self, id: &str) -> Result<(), RagError>;

    // File operations
    async fn upload_file(
        &self,
        filename: &str,
        content: Vec<u8>,
        purpose: &str,
    ) -> Result<RagFile, RagError>;

    async fn attach_file_to_store(
        &self,
        vector_store_id: &str,
        file_id: &str,
    ) -> Result<(), RagError>;

    async fn delete_file(&self, file_id: &str) -> Result<(), RagError>;

    // Search
    async fn search(
        &self,
        vector_store_id: &str,
        query: &str,
        max_results: Option<u32>,
    ) -> Result<Vec<SearchResult>, RagError>;
}
