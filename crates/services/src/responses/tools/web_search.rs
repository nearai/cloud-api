//! Web Search Tool Executor
//!
//! Implements the ToolExecutor trait for web search functionality.

use async_trait::async_trait;
use std::sync::Arc;

use super::executor::{ToolEventContext, ToolExecutionContext, ToolExecutor, ToolOutput};
use super::ports::{WebSearchParams, WebSearchProviderTrait, WebSearchResult};
use crate::responses::errors::ResponseError;
use crate::responses::models::{ResponseItemStatus, ResponseOutputItem, WebSearchAction};
use crate::responses::service_helpers::ToolCallInfo;

/// Create a web search output item for streaming events
fn create_web_search_item(
    event_ctx: &ToolEventContext<'_>,
    query: &str,
    status: ResponseItemStatus,
) -> ResponseOutputItem {
    ResponseOutputItem::WebSearchCall {
        id: event_ctx.tool_call_id.to_string(),
        response_id: event_ctx.stream_ctx.response_id_str.clone(),
        previous_response_id: event_ctx.stream_ctx.previous_response_id.clone(),
        next_response_ids: Vec::new(),
        created_at: event_ctx.stream_ctx.created_at,
        status,
        action: WebSearchAction::Search {
            query: query.to_string(),
        },
        model: event_ctx.stream_ctx.model.clone(),
    }
}

pub const WEB_SEARCH_TOOL_NAME: &str = "web_search";

/// Citation instruction provided on first web search.
/// Exported for use by the service layer when accumulating sources.
pub const CITATION_INSTRUCTION: &str = r#"CITATION REQUIREMENT: Use [s:N]text[/s:N] for EVERY fact from web search results.

FORMAT: [s:N]fact from source N[/s:N]
- N = source number (0, 1, 2, 3, etc. - cumulative across all searches)
- ALWAYS use BOTH opening [s:N] and closing [/s:N] tags together
- The number N MUST match in opening and closing tags
- Cite specific facts, names, numbers, and statements from sources
- Every factual claim must be wrapped

CORRECT EXAMPLES:
[s:0]San Francisco's top restaurant is The French Laundry[/s:0]
[s:1]The app TikTok has over 2 billion downloads[/s:1]
[s:2]Instagram was founded in 2010[/s:2]

DO NOT USE THESE FORMATS:
✗ [s:0]Missing closing tag
✗ [s:0]Mismatched[/s:1] numbers
✗ Statements without any citation tags"#;

/// Format search results with the given start index
pub fn format_search_results(results: &[WebSearchResult], start_index: usize) -> String {
    results
        .iter()
        .enumerate()
        .map(|(idx, r)| {
            format!(
                "Source: {}\nTitle: {}\nURL: {}\nSnippet: {}\n",
                start_index + idx,
                r.title,
                r.url,
                r.snippet
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Web search tool executor
///
/// Executes web searches via a provider and returns results.
/// The service layer handles citation tracking and source accumulation.
pub struct WebSearchToolExecutor {
    provider: Arc<dyn WebSearchProviderTrait>,
}

impl WebSearchToolExecutor {
    /// Create a new web search executor with the given provider
    pub fn new(provider: Arc<dyn WebSearchProviderTrait>) -> Self {
        Self { provider }
    }

    /// Parse additional parameters from the tool call
    fn parse_params(tool_call: &ToolCallInfo) -> WebSearchParams {
        let mut search_params = WebSearchParams::new(tool_call.query.clone());

        if let Some(params) = &tool_call.params {
            if let Some(country) = params.get("country").and_then(|v| v.as_str()) {
                search_params.country = Some(country.to_string());
            }
            if let Some(lang) = params.get("search_lang").and_then(|v| v.as_str()) {
                search_params.search_lang = Some(lang.to_string());
            }
            if let Some(ui_lang) = params.get("ui_lang").and_then(|v| v.as_str()) {
                search_params.ui_lang = Some(ui_lang.to_string());
            }
            if let Some(count) = params.get("count").and_then(|v| v.as_u64()) {
                search_params.count = Some(count as u32);
            }
            if let Some(offset) = params.get("offset").and_then(|v| v.as_u64()) {
                search_params.offset = Some(offset as u32);
            }
            if let Some(safesearch) = params.get("safesearch").and_then(|v| v.as_str()) {
                search_params.safesearch = Some(safesearch.to_string());
            }
            if let Some(freshness) = params.get("freshness").and_then(|v| v.as_str()) {
                search_params.freshness = Some(freshness.to_string());
            }
            if let Some(text_decorations) = params.get("text_decorations").and_then(|v| v.as_bool())
            {
                search_params.text_decorations = Some(text_decorations);
            }
            if let Some(spellcheck) = params.get("spellcheck").and_then(|v| v.as_bool()) {
                search_params.spellcheck = Some(spellcheck);
            }
            if let Some(units) = params.get("units").and_then(|v| v.as_str()) {
                search_params.units = Some(units.to_string());
            }
            if let Some(extra_snippets) = params.get("extra_snippets").and_then(|v| v.as_bool()) {
                search_params.extra_snippets = Some(extra_snippets);
            }
            if let Some(summary) = params.get("summary").and_then(|v| v.as_bool()) {
                search_params.summary = Some(summary);
            }
        }

        search_params
    }
}

#[async_trait]
impl ToolExecutor for WebSearchToolExecutor {
    fn name(&self) -> &str {
        WEB_SEARCH_TOOL_NAME
    }

    fn can_handle(&self, tool_name: &str) -> bool {
        tool_name == WEB_SEARCH_TOOL_NAME
    }

    async fn execute(
        &self,
        tool_call: &ToolCallInfo,
        _context: &ToolExecutionContext<'_>,
    ) -> Result<ToolOutput, ResponseError> {
        let search_params = Self::parse_params(tool_call);

        let sources = self
            .provider
            .search(search_params)
            .await
            .map_err(|e| ResponseError::InternalError(format!("Web search failed: {e}")))?;

        let formatted = format_search_results(&sources, 0);

        Ok(ToolOutput::WebSearch { formatted, sources })
    }

    async fn emit_start(
        &self,
        tool_call: &ToolCallInfo,
        event_ctx: &mut ToolEventContext<'_>,
    ) -> Result<(), ResponseError> {
        let item =
            create_web_search_item(event_ctx, &tool_call.query, ResponseItemStatus::InProgress);
        event_ctx.emit_item_added(item).await?;

        event_ctx
            .emit_simple_event("response.web_search_call.in_progress")
            .await?;

        event_ctx
            .emit_simple_event("response.web_search_call.searching")
            .await?;

        Ok(())
    }

    async fn emit_complete(
        &self,
        tool_call: &ToolCallInfo,
        event_ctx: &mut ToolEventContext<'_>,
    ) -> Result<(), ResponseError> {
        event_ctx
            .emit_simple_event("response.web_search_call.completed")
            .await?;

        let item =
            create_web_search_item(event_ctx, &tool_call.query, ResponseItemStatus::Completed);
        event_ctx.emit_item_done(item).await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses::models::CreateResponseRequest;
    use crate::responses::tools::ports::WebSearchError;

    struct MockWebSearchProvider {
        results: Vec<WebSearchResult>,
    }

    #[async_trait]
    impl WebSearchProviderTrait for MockWebSearchProvider {
        async fn search(
            &self,
            _params: WebSearchParams,
        ) -> Result<Vec<WebSearchResult>, WebSearchError> {
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

    #[tokio::test]
    async fn test_web_search_returns_sources() {
        let provider = Arc::new(MockWebSearchProvider {
            results: vec![WebSearchResult {
                title: "Test".to_string(),
                url: "https://example.com".to_string(),
                snippet: "Test snippet".to_string(),
            }],
        });

        let executor = WebSearchToolExecutor::new(provider);
        let tool_call = ToolCallInfo {
            tool_type: WEB_SEARCH_TOOL_NAME.to_string(),
            query: "test query".to_string(),
            params: None,
        };

        let request = create_test_request();
        let context = ToolExecutionContext { request: &request };

        let result = executor.execute(&tool_call, &context).await.unwrap();

        match result {
            ToolOutput::WebSearch { formatted, sources } => {
                assert!(formatted.contains("Source: 0"));
                assert!(formatted.contains("Test"));
                assert_eq!(sources.len(), 1);
                assert_eq!(sources[0].title, "Test");
            }
            _ => panic!("Expected WebSearch output"),
        }
    }

    #[tokio::test]
    async fn test_web_search_parses_params() {
        let provider = Arc::new(MockWebSearchProvider { results: vec![] });
        let executor = WebSearchToolExecutor::new(provider);

        let tool_call = ToolCallInfo {
            tool_type: WEB_SEARCH_TOOL_NAME.to_string(),
            query: "test query".to_string(),
            params: Some(serde_json::json!({
                "country": "US",
                "count": 10,
                "safesearch": "moderate"
            })),
        };

        let request = create_test_request();
        let context = ToolExecutionContext { request: &request };

        // Just verify it doesn't error - param parsing happens internally
        let result = executor.execute(&tool_call, &context).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_can_handle() {
        let provider = Arc::new(MockWebSearchProvider { results: vec![] });
        let executor = WebSearchToolExecutor::new(provider);

        assert!(executor.can_handle("web_search"));
        assert!(!executor.can_handle("file_search"));
        assert!(!executor.can_handle("mcp:tool"));
    }

    #[test]
    fn test_format_results_with_offset() {
        let results = vec![
            WebSearchResult {
                title: "First".to_string(),
                url: "https://first.com".to_string(),
                snippet: "First snippet".to_string(),
            },
            WebSearchResult {
                title: "Second".to_string(),
                url: "https://second.com".to_string(),
                snippet: "Second snippet".to_string(),
            },
        ];

        let formatted = format_search_results(&results, 3);
        assert!(formatted.contains("Source: 3"));
        assert!(formatted.contains("Source: 4"));
    }
}
