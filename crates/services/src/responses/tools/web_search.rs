//! Web Search Tool Executor
//!
//! Implements the ToolExecutor trait for web search functionality.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use super::executor::{ToolEventContext, ToolExecutionContext, ToolExecutor, ToolOutput};
use super::ports::{WebSearchParams, WebSearchProviderTrait, WebSearchResult};
use crate::responses::errors::ResponseError;
use crate::responses::models::{ResponseItemStatus, ResponseOutputItem, WebSearchAction};
use crate::responses::service_helpers::ToolCallInfo;
use crate::web_search::{WEB_SEARCH_MAX_COUNT, WEB_SEARCH_MAX_OFFSET};

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

/// Shared JSON Schema for Brave-backed web search parameters.
/// Keep this in one place so MCP tool exposure and model-facing tool definitions stay aligned.
pub fn web_search_parameters_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Brave web search query string. Put search operators directly in this field, for example exact phrases in quotes, `site:github.com rust tutorials`, `filetype:pdf`, or excluded terms like `-jquery`.",
                "examples": [
                    "machine learning tutorials",
                    "site:github.com rust tutorials",
                    "\"climate change solutions\" filetype:pdf -policy"
                ]
            },
            "country": {
                "type": "string",
                "description": "2-character country code used to target results from a specific country, for example `US`, `GB`, or `DE`.",
                "minLength": 2,
                "maxLength": 2,
                "examples": ["US", "DE"]
            },
            "search_lang": {
                "type": "string",
                "description": "Preferred language of matching documents/content, typically a 2-character language code such as `en`, `es`, or `de`.",
                "minLength": 2,
                "examples": ["en", "de"]
            },
            "ui_lang": {
                "type": "string",
                "description": "Preferred language/locale for response metadata, for example `en-US` or `de-DE`.",
                "examples": ["en-US", "de-DE"]
            },
            "count": {
                "type": "integer",
                "description": "Maximum number of results to return in one page. Brave documents a maximum of 20, and the actual number returned may be smaller.",
                "minimum": 1,
                "maximum": WEB_SEARCH_MAX_COUNT,
                "examples": [5, 10, 20]
            },
            "offset": {
                "type": "integer",
                "description": "Zero-based pagination offset. Brave documents a maximum of 9; request the next page only when Brave indicates more results are available.",
                "minimum": 0,
                "maximum": WEB_SEARCH_MAX_OFFSET,
                "examples": [0, 1, 2]
            },
            "safesearch": {
                "type": "string",
                "description": "Adult-content filtering level. `moderate` is Brave's documented default.",
                "enum": ["off", "moderate", "strict"]
            },
            "freshness": {
                "type": "string",
                "description": "Time filter for results: `pd` (last 24h), `pw` (last 7d), `pm` (last 31d), `py` (last 365d), or a custom date range formatted as `YYYY-MM-DDtoYYYY-MM-DD`.",
                "examples": ["pd", "pw", "pm", "py", "2022-04-01to2022-07-30"]
            },
            "text_decorations": {
                "type": "boolean",
                "description": "Whether Brave should include text-decoration/highlighting markers in returned snippets."
            },
            "spellcheck": {
                "type": "boolean",
                "description": "Whether Brave should apply query spellchecking/correction before searching."
            },
            "units": {
                "type": "string",
                "description": "Measurement units for responses that include unit-bearing data.",
                "enum": ["metric", "imperial"]
            },
            "extra_snippets": {
                "type": "boolean",
                "description": "When true, request up to 5 additional excerpts per result to provide richer previews."
            },
            "summary": {
                "type": "boolean",
                "description": "Request Brave summary-key generation when supported by the upstream API/plan."
            },
            "result_filter": {
                "type": "string",
                "description": "Comma-delimited result types to include. Supported pass-through values are `discussions`, `faq`, `infobox`, `news`, `query`, `summarizer`, `videos`, `web`, and `locations`.",
                "examples": ["web", "web,news", "locations,web"]
            },
            "goggles": {
                "type": "string",
                "description": "Brave Goggles URL or inline definition used for custom re-ranking/filtering. Brave documentation notes goggles may be provided as hosted URLs or inline definitions.",
                "examples": [
                    "https://example.com/my.goggle",
                    "https://example.com/one.goggle,https://example.com/two.goggle"
                ]
            },
            "enable_rich_callback": {
                "type": "boolean",
                "description": "Ask Brave to include rich-result callback hints for supported intents such as weather, sports, or stocks. If available, the response contains a `rich.hint.callback_key` for follow-up `/web/rich` fetches."
            },
            "include_fetch_metadata": {
                "type": "boolean",
                "description": "Pass-through flag requesting additional fetch metadata in Brave responses when available."
            },
            "operators": {
                "type": "boolean",
                "description": "Optional pass-through flag for operator handling. Search operators themselves still belong directly inside `query`."
            }
        },
        "required": ["query"],
        "additionalProperties": false
    })
}

/// Citation instruction provided on first web search.
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

/// Result of formatting web search results
pub struct FormattedWebSearchResult {
    /// The formatted content for LLM consumption
    pub formatted: String,
    /// Citation instruction (only on first search)
    pub instruction: Option<String>,
}

/// Format web search results with proper source indexing
pub fn format_results(
    sources: &[WebSearchResult],
    current_source_count: usize,
) -> FormattedWebSearchResult {
    let formatted = sources
        .iter()
        .enumerate()
        .map(|(idx, r)| {
            format!(
                "Source: {}\nTitle: {}\nURL: {}\nSnippet: {}\n",
                current_source_count + idx,
                r.title,
                r.url,
                r.snippet
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let instruction = if current_source_count == 0 {
        Some(CITATION_INSTRUCTION.to_string())
    } else {
        None
    };

    FormattedWebSearchResult {
        formatted,
        instruction,
    }
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
            if let Some(rf) = params.get("result_filter").and_then(|v| v.as_str()) {
                search_params.result_filter = Some(rf.to_string());
            }
            if let Some(g) = params.get("goggles").and_then(|v| v.as_str()) {
                search_params.goggles = Some(g.to_string());
            }
            if let Some(erc) = params.get("enable_rich_callback").and_then(|v| v.as_bool()) {
                search_params.enable_rich_callback = Some(erc);
            }
            if let Some(ifm) = params
                .get("include_fetch_metadata")
                .and_then(|v| v.as_bool())
            {
                search_params.include_fetch_metadata = Some(ifm);
            }
            if let Some(op) = params.get("operators").and_then(|v| v.as_bool()) {
                search_params.operators = Some(op);
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

        Ok(ToolOutput::WebSearch { sources })
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
    use std::sync::{Arc as StdArc, Mutex};

    struct MockWebSearchProvider {
        results: Vec<WebSearchResult>,
        last_params: StdArc<Mutex<Option<WebSearchParams>>>,
    }

    #[async_trait]
    impl WebSearchProviderTrait for MockWebSearchProvider {
        async fn search(
            &self,
            params: WebSearchParams,
        ) -> Result<Vec<WebSearchResult>, WebSearchError> {
            // Capture the last params for inspection in tests
            if let Ok(mut guard) = self.last_params.lock() {
                *guard = Some(params);
            }
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
        let last_params = StdArc::new(Mutex::new(None));
        let provider = Arc::new(MockWebSearchProvider {
            results: vec![WebSearchResult {
                title: "Test".to_string(),
                url: "https://example.com".to_string(),
                snippet: "Test snippet".to_string(),
            }],
            last_params,
        });

        let executor = WebSearchToolExecutor::new(provider);
        let tool_call = ToolCallInfo {
            id: None,
            tool_type: WEB_SEARCH_TOOL_NAME.to_string(),
            query: "test query".to_string(),
            params: None,
            thought_signature: None,
        };

        let request = create_test_request();
        let context = ToolExecutionContext { request: &request };

        let result = executor.execute(&tool_call, &context).await.unwrap();

        match result {
            ToolOutput::WebSearch { sources } => {
                assert_eq!(sources.len(), 1);
                assert_eq!(sources[0].title, "Test");
                assert_eq!(sources[0].url, "https://example.com");
            }
            _ => panic!("Expected WebSearch output"),
        }
    }

    #[tokio::test]
    async fn test_web_search_parses_params() {
        let last_params = StdArc::new(Mutex::new(None));
        let provider = Arc::new(MockWebSearchProvider {
            results: vec![],
            last_params: last_params.clone(),
        });
        let executor = WebSearchToolExecutor::new(provider);

        let tool_call = ToolCallInfo {
            id: None,
            tool_type: WEB_SEARCH_TOOL_NAME.to_string(),
            query: "test query".to_string(),
            params: Some(serde_json::json!({
                "country": "US",
                "count": 10,
                "safesearch": "moderate",
                "result_filter": "web,news",
                "goggles": "https://example.com/my-goggles",
                "enable_rich_callback": true,
                "include_fetch_metadata": true,
                "operators": true
            })),
            thought_signature: None,
        };

        let request = create_test_request();
        let context = ToolExecutionContext { request: &request };

        let result = executor.execute(&tool_call, &context).await;
        assert!(
            result.is_ok(),
            "executor should succeed with extended params"
        );

        // Verify that all parameters were parsed and passed through to the provider.
        let guard = last_params.lock().expect("lock last_params");
        let params = guard.as_ref().expect("expected captured params");
        assert_eq!(params.country.as_deref(), Some("US"));
        assert_eq!(params.count, Some(10));
        assert_eq!(params.safesearch.as_deref(), Some("moderate"));
        assert_eq!(params.result_filter.as_deref(), Some("web,news"));
        assert_eq!(
            params.goggles.as_deref(),
            Some("https://example.com/my-goggles")
        );
        assert_eq!(params.enable_rich_callback, Some(true));
        assert_eq!(params.include_fetch_metadata, Some(true));
        assert_eq!(params.operators, Some(true));
    }

    #[test]
    fn test_can_handle() {
        let provider = Arc::new(MockWebSearchProvider {
            results: vec![],
            last_params: StdArc::new(Mutex::new(None)),
        });
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

        let result = format_results(&results, 3);
        assert!(result.formatted.contains("Source: 3"));
        assert!(result.formatted.contains("Source: 4"));
        // Not first search, so no instruction
        assert!(result.instruction.is_none());
    }

    #[test]
    fn test_format_results_first_search_includes_instruction() {
        let results = vec![WebSearchResult {
            title: "Test".to_string(),
            url: "https://test.com".to_string(),
            snippet: "Test snippet".to_string(),
        }];

        let result = format_results(&results, 0);
        assert!(result.formatted.contains("Source: 0"));
        // First search should include citation instruction
        assert!(result.instruction.is_some());
        assert!(result.instruction.unwrap().contains("CITATION REQUIREMENT"));
    }
}
