use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::conversations::models::ConversationId;

/// Result from a web search
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Result from a file search
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSearchResult {
    pub file_id: String,
    pub file_name: String,
    pub content: String,
    pub relevance_score: f32,
}

#[derive(Debug, thiserror::Error)]
pub enum WebSearchError {
    #[error("Web search failed: {0}")]
    WebSearchRequestFailed(String),
    #[error("Web search response parsing failed: {0}")]
    WebSearchResponseParsingFailed(String),
}

/// Web search provider trait
#[async_trait]
pub trait WebSearchProviderTrait: Send + Sync {
    /// Perform a web search with the given query
    async fn search(&self, query: String) -> Result<Vec<WebSearchResult>, WebSearchError>;
}

#[derive(Debug, thiserror::Error)]
pub enum FileSearchError {
    #[error("File search failed: {0}")]
    FileSearchFailed(String),
}

/// File search provider trait
#[async_trait]
pub trait FileSearchProviderTrait: Send + Sync {
    /// Search files within a conversation
    async fn search_conversation_files(
        &self,
        conversation_id: ConversationId,
        query: String,
    ) -> Result<Vec<FileSearchResult>, FileSearchError>;
}
