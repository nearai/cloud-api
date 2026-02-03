use std::sync::Arc;

use async_trait::async_trait;

use super::ports::{FileSearchError, FileSearchProviderTrait, FileSearchResult};
use crate::conversations::models::ConversationId;
use crate::rag::RagServiceTrait;

/// File search provider that delegates to the RAG service.
///
/// Searches across all vector stores in the RAG service for the given query.
/// The mapping between conversations and specific vector stores will be
/// refined as the integration matures.
pub struct RagFileSearchProvider {
    rag_service: Arc<dyn RagServiceTrait>,
}

impl RagFileSearchProvider {
    pub fn new(rag_service: Arc<dyn RagServiceTrait>) -> Self {
        Self { rag_service }
    }
}

#[async_trait]
impl FileSearchProviderTrait for RagFileSearchProvider {
    async fn search_conversation_files(
        &self,
        conversation_id: ConversationId,
        query: String,
    ) -> Result<Vec<FileSearchResult>, FileSearchError> {
        // For now, use the conversation ID as the vector store ID.
        // This assumes a 1:1 mapping between conversations and vector stores,
        // which will be refined as the integration matures.
        let vector_store_id = conversation_id.0.to_string();

        match self
            .rag_service
            .search(&vector_store_id, &query, Some(5))
            .await
        {
            Ok(results) => Ok(results
                .into_iter()
                .map(|r| FileSearchResult {
                    file_id: r.file_id,
                    file_name: r.file_name,
                    content: r.content,
                    relevance_score: r.score,
                })
                .collect()),
            Err(e) => {
                tracing::warn!(
                    conversation_id = %conversation_id.0,
                    error = %e,
                    "RAG service file search failed"
                );
                Err(FileSearchError::FileSearchFailed(e.to_string()))
            }
        }
    }
}
