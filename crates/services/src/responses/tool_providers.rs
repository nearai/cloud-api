use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::conversations::models::ConversationId;

// ============================================
// Tool Provider Traits
// ============================================

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

/// Web search provider trait
#[async_trait]
pub trait WebSearchProviderTrait: Send + Sync {
    /// Perform a web search with the given query
    async fn search(&self, query: String) -> anyhow::Result<Vec<WebSearchResult>>;
}

/// File search provider trait
#[async_trait]
pub trait FileSearchProviderTrait: Send + Sync {
    /// Search files within a conversation
    async fn search_conversation_files(
        &self,
        conversation_id: ConversationId,
        query: String,
    ) -> anyhow::Result<Vec<FileSearchResult>>;
}
