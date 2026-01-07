use async_trait::async_trait;
use std::sync::Arc;

use super::executor::{ToolExecutionContext, ToolExecutor, ToolOutput};
use super::ports::{FileSearchProviderTrait, FileSearchResult};
use crate::conversations::models::ConversationId;
use crate::id_prefixes::PREFIX_CONV;
use crate::responses::errors::ResponseError;
use crate::responses::models::ConversationReference;
use crate::responses::service_helpers::ToolCallInfo;

pub const FILE_SEARCH_TOOL_NAME: &str = "file_search";

/// File search tool executor
///
/// Executes file searches within a conversation context via a provider.
pub struct FileSearchToolExecutor {
    provider: Arc<dyn FileSearchProviderTrait>,
}

impl FileSearchToolExecutor {
    /// Create a new file search executor with the given provider
    pub fn new(provider: Arc<dyn FileSearchProviderTrait>) -> Self {
        Self { provider }
    }

    /// Extract conversation ID from the request
    fn extract_conversation_id(
        conversation: &Option<ConversationReference>,
    ) -> Result<Option<uuid::Uuid>, ResponseError> {
        match conversation {
            Some(ConversationReference::Id(id)) => {
                let uuid_str = id.strip_prefix(PREFIX_CONV).unwrap_or(id);
                let uuid = uuid::Uuid::parse_str(uuid_str).map_err(|e| {
                    ResponseError::InvalidParams(format!("Invalid conversation ID: {e}"))
                })?;
                Ok(Some(uuid))
            }
            Some(ConversationReference::Object { id, .. }) => {
                let uuid_str = id.strip_prefix(PREFIX_CONV).unwrap_or(id);
                let uuid = uuid::Uuid::parse_str(uuid_str).map_err(|e| {
                    ResponseError::InvalidParams(format!("Invalid conversation ID: {e}"))
                })?;
                Ok(Some(uuid))
            }
            None => Ok(None),
        }
    }

    /// Format file search results for the model
    pub fn format_results(results: &[FileSearchResult]) -> String {
        results
            .iter()
            .map(|r| {
                format!(
                    "File: {}\nContent: {}\nRelevance: {}\n",
                    r.file_name, r.content, r.relevance_score
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[async_trait]
impl ToolExecutor for FileSearchToolExecutor {
    fn name(&self) -> &str {
        FILE_SEARCH_TOOL_NAME
    }

    fn can_handle(&self, tool_name: &str) -> bool {
        tool_name == FILE_SEARCH_TOOL_NAME
    }

    async fn execute(
        &self,
        tool_call: &ToolCallInfo,
        context: &ToolExecutionContext<'_>,
    ) -> Result<ToolOutput, ResponseError> {
        // Get conversation ID from request
        let conversation_id = match Self::extract_conversation_id(&context.request.conversation)? {
            Some(id) => id,
            None => {
                return Ok(ToolOutput::Text(
                    "File search requires a conversation context".to_string(),
                ));
            }
        };

        let results = self
            .provider
            .search_conversation_files(
                ConversationId::from(conversation_id),
                tool_call.query.clone(),
            )
            .await
            .map_err(|e| ResponseError::InternalError(format!("File search failed: {e}")))?;

        Ok(ToolOutput::FileSearch { results })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses::models::CreateResponseRequest;
    use crate::responses::tools::ports::FileSearchError;

    struct MockFileSearchProvider {
        results: Vec<FileSearchResult>,
    }

    #[async_trait]
    impl FileSearchProviderTrait for MockFileSearchProvider {
        async fn search_conversation_files(
            &self,
            _conversation_id: ConversationId,
            _query: String,
        ) -> Result<Vec<FileSearchResult>, FileSearchError> {
            Ok(self.results.clone())
        }
    }

    fn create_test_request() -> CreateResponseRequest {
        CreateResponseRequest {
            model: "test".to_string(),
            input: None,
            instructions: None,
            conversation: None,
            previous_response_id: None,
            max_output_tokens: None,
            max_tool_calls: None,
            temperature: None,
            top_p: None,
            stream: None,
            store: None,
            background: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            reasoning: None,
            include: None,
            metadata: None,
            safety_identifier: None,
            prompt_cache_key: None,
        }
    }

    fn create_request_with_conversation(conv_id: &str) -> CreateResponseRequest {
        CreateResponseRequest {
            model: "test".to_string(),
            input: None,
            instructions: None,
            conversation: Some(ConversationReference::Id(conv_id.to_string())),
            previous_response_id: None,
            max_output_tokens: None,
            max_tool_calls: None,
            temperature: None,
            top_p: None,
            stream: None,
            store: None,
            background: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            reasoning: None,
            include: None,
            metadata: None,
            safety_identifier: None,
            prompt_cache_key: None,
        }
    }

    #[tokio::test]
    async fn test_file_search_requires_conversation() {
        let provider = Arc::new(MockFileSearchProvider { results: vec![] });
        let executor = FileSearchToolExecutor::new(provider);

        let tool_call = ToolCallInfo {
            tool_type: FILE_SEARCH_TOOL_NAME.to_string(),
            query: "test query".to_string(),
            params: None,
        };

        let request = create_test_request(); // No conversation
        let context = ToolExecutionContext { request: &request };

        let result = executor.execute(&tool_call, &context).await.unwrap();
        match result {
            ToolOutput::Text(msg) => {
                assert!(msg.contains("requires a conversation context"));
            }
            _ => panic!("Expected Text output for missing conversation"),
        }
    }

    #[tokio::test]
    async fn test_file_search_with_conversation() {
        let provider = Arc::new(MockFileSearchProvider {
            results: vec![FileSearchResult {
                file_id: "file_123".to_string(),
                file_name: "test.txt".to_string(),
                content: "Test content".to_string(),
                relevance_score: 0.95,
            }],
        });

        let executor = FileSearchToolExecutor::new(provider);
        let tool_call = ToolCallInfo {
            tool_type: FILE_SEARCH_TOOL_NAME.to_string(),
            query: "test query".to_string(),
            params: None,
        };

        let conv_uuid = uuid::Uuid::new_v4();
        let request = create_request_with_conversation(&format!("conv_{}", conv_uuid));
        let context = ToolExecutionContext { request: &request };

        let result = executor.execute(&tool_call, &context).await.unwrap();
        match result {
            ToolOutput::FileSearch { results } => {
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].file_name, "test.txt");
                assert_eq!(results[0].content, "Test content");
            }
            _ => panic!("Expected FileSearch output"),
        }
    }

    #[test]
    fn test_can_handle() {
        let provider = Arc::new(MockFileSearchProvider { results: vec![] });
        let executor = FileSearchToolExecutor::new(provider);

        assert!(executor.can_handle("file_search"));
        assert!(!executor.can_handle("web_search"));
        assert!(!executor.can_handle("mcp:tool"));
    }

    #[test]
    fn test_format_results() {
        let results = vec![
            FileSearchResult {
                file_id: "1".to_string(),
                file_name: "file1.txt".to_string(),
                content: "Content 1".to_string(),
                relevance_score: 0.9,
            },
            FileSearchResult {
                file_id: "2".to_string(),
                file_name: "file2.txt".to_string(),
                content: "Content 2".to_string(),
                relevance_score: 0.8,
            },
        ];

        let formatted = FileSearchToolExecutor::format_results(&results);
        assert!(formatted.contains("file1.txt"));
        assert!(formatted.contains("file2.txt"));
        assert!(formatted.contains("0.9"));
        assert!(formatted.contains("0.8"));
    }
}
