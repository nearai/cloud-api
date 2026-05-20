//! Context-optimized Web Search Tool Executor
//!
//! Implements a Brave LLM Context backed search tool for Responses API.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;

use super::executor::{ToolEventContext, ToolExecutionContext, ToolExecutor, ToolOutput};
use super::ports::{WebContextSearchParams, WebContextSearchProviderTrait};
use crate::responses::errors::ResponseError;
use crate::responses::models::{ResponseItemStatus, ResponseOutputItem, WebSearchAction};
use crate::responses::service_helpers::ToolCallInfo;

pub const WEB_CONTEXT_SEARCH_TOOL_NAME: &str = "web_context_search";

const DEFAULT_COUNT: u32 = 20;
const MAX_COUNT: u32 = 50;
const DEFAULT_SPELLCHECK: bool = true;
const DEFAULT_MAX_URLS: u32 = 20;
const MAX_URLS: u32 = 50;
const MIN_TOKENS: u32 = 1024;
const DEFAULT_MAX_TOKENS: u32 = 8192;
const MAX_TOKENS: u32 = 32768;
const DEFAULT_MAX_SNIPPETS: u32 = 50;
const MAX_SNIPPETS: u32 = 100;
const MIN_TOKENS_PER_URL: u32 = 512;
const DEFAULT_MAX_TOKENS_PER_URL: u32 = 4096;
const MAX_TOKENS_PER_URL: u32 = 8192;
const DEFAULT_MAX_SNIPPETS_PER_URL: u32 = 50;
const MAX_SNIPPETS_PER_URL: u32 = 100;
const DEFAULT_THRESHOLD_MODE: &str = "balanced";

fn clamp_range(value: Option<u64>, default: u32, min: u32, max: u32) -> u32 {
    value
        .map(|value| value.clamp(u64::from(min), u64::from(max)) as u32)
        .unwrap_or(default)
}

fn clamp_nonzero(value: Option<u64>, default: u32, max: u32) -> u32 {
    clamp_range(value, default, 1, max)
}

fn normalize_threshold_mode(value: Option<&str>) -> String {
    match value {
        Some("disabled") => "disabled".to_string(),
        Some("strict") => "strict".to_string(),
        Some("balanced") => "balanced".to_string(),
        Some("lenient") => "lenient".to_string(),
        _ => DEFAULT_THRESHOLD_MODE.to_string(),
    }
}

fn create_web_context_search_item(
    event_ctx: &ToolEventContext<'_>,
    query: &str,
    status: ResponseItemStatus,
) -> ResponseOutputItem {
    // Reuse the standard Responses web-search event shape so clients and citations stay compatible.
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

/// JSON Schema for context-optimized Brave search parameters.
pub fn web_context_search_parameters_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Search query for retrieving source-grounded web context."
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
            "freshness": {
                "type": "string",
                "description": "Time filter for results: `pd` (last 24h), `pw` (last 7d), `pm` (last 31d), `py` (last 365d), or a custom date range formatted as `YYYY-MM-DDtoYYYY-MM-DD`.",
                "examples": ["pd", "pw", "pm", "py", "2022-04-01to2022-07-30"]
            },
            "spellcheck": {
                "type": "boolean",
                "description": "Whether Brave should apply spellcheck to the query.",
                "default": DEFAULT_SPELLCHECK
            },
            "count": {
                "type": "integer",
                "description": "Maximum number of search results Brave should consider for context retrieval.",
                "minimum": 1,
                "maximum": MAX_COUNT,
                "default": DEFAULT_COUNT,
                "examples": [5, 10, 20]
            },
            "maximum_number_of_urls": {
                "type": "integer",
                "description": "Maximum number of source URLs to include in the returned context.",
                "minimum": 1,
                "maximum": MAX_URLS,
                "default": DEFAULT_MAX_URLS,
                "examples": [3, 5, 10]
            },
            "maximum_number_of_tokens": {
                "type": "integer",
                "description": "Approximate maximum number of tokens to include in the returned context.",
                "minimum": MIN_TOKENS,
                "maximum": MAX_TOKENS,
                "default": DEFAULT_MAX_TOKENS,
                "examples": [2048, 4096, 8192]
            },
            "maximum_number_of_snippets": {
                "type": "integer",
                "description": "Maximum number of snippets to include across all source URLs.",
                "minimum": 1,
                "maximum": MAX_SNIPPETS,
                "default": DEFAULT_MAX_SNIPPETS,
                "examples": [10, 25, 50]
            },
            "maximum_number_of_tokens_per_url": {
                "type": "integer",
                "description": "Maximum number of tokens to include per individual URL.",
                "minimum": MIN_TOKENS_PER_URL,
                "maximum": MAX_TOKENS_PER_URL,
                "default": DEFAULT_MAX_TOKENS_PER_URL,
                "examples": [1024, 2048, 4096]
            },
            "maximum_number_of_snippets_per_url": {
                "type": "integer",
                "description": "Maximum number of snippets to include per individual URL.",
                "minimum": 1,
                "maximum": MAX_SNIPPETS_PER_URL,
                "default": DEFAULT_MAX_SNIPPETS_PER_URL,
                "examples": [5, 10, 25]
            },
            "context_threshold_mode": {
                "type": "string",
                "description": "Relevance threshold for including content. Use `strict` for higher precision, `lenient` for broader recall, `disabled` for no threshold filtering, or `balanced` by default.",
                "enum": ["disabled", "strict", "balanced", "lenient"],
                "default": DEFAULT_THRESHOLD_MODE
            }
        },
        "required": ["query"],
        "additionalProperties": false
    })
}

/// Context search tool executor.
pub struct WebContextSearchToolExecutor {
    provider: Arc<dyn WebContextSearchProviderTrait>,
}

impl WebContextSearchToolExecutor {
    pub fn new(provider: Arc<dyn WebContextSearchProviderTrait>) -> Self {
        Self { provider }
    }

    fn parse_params(tool_call: &ToolCallInfo) -> WebContextSearchParams {
        let mut search_params = WebContextSearchParams {
            query: tool_call.query.clone(),
            spellcheck: Some(DEFAULT_SPELLCHECK),
            count: Some(DEFAULT_COUNT),
            maximum_number_of_urls: Some(DEFAULT_MAX_URLS),
            maximum_number_of_tokens: Some(DEFAULT_MAX_TOKENS),
            maximum_number_of_snippets: Some(DEFAULT_MAX_SNIPPETS),
            maximum_number_of_tokens_per_url: Some(DEFAULT_MAX_TOKENS_PER_URL),
            maximum_number_of_snippets_per_url: Some(DEFAULT_MAX_SNIPPETS_PER_URL),
            context_threshold_mode: Some(DEFAULT_THRESHOLD_MODE.to_string()),
            ..Default::default()
        };

        if let Some(params) = &tool_call.params {
            if let Some(country) = params.get("country").and_then(|v| v.as_str()) {
                search_params.country = Some(country.to_string());
            }
            if let Some(lang) = params.get("search_lang").and_then(|v| v.as_str()) {
                search_params.search_lang = Some(lang.to_string());
            }
            if let Some(freshness) = params.get("freshness").and_then(|v| v.as_str()) {
                search_params.freshness = Some(freshness.to_string());
            }

            if let Some(value) = params.get("spellcheck").and_then(|v| v.as_bool()) {
                search_params.spellcheck = Some(value);
            }
            if let Some(value) = params.get("count").and_then(|v| v.as_u64()) {
                search_params.count = Some(clamp_nonzero(Some(value), DEFAULT_COUNT, MAX_COUNT));
            }
            if let Some(value) = params
                .get("maximum_number_of_urls")
                .and_then(|v| v.as_u64())
            {
                search_params.maximum_number_of_urls =
                    Some(clamp_nonzero(Some(value), DEFAULT_MAX_URLS, MAX_URLS));
            }
            if let Some(value) = params
                .get("maximum_number_of_tokens")
                .and_then(|v| v.as_u64())
            {
                search_params.maximum_number_of_tokens = Some(clamp_range(
                    Some(value),
                    DEFAULT_MAX_TOKENS,
                    MIN_TOKENS,
                    MAX_TOKENS,
                ));
            }
            if let Some(value) = params
                .get("maximum_number_of_snippets")
                .and_then(|v| v.as_u64())
            {
                search_params.maximum_number_of_snippets = Some(clamp_nonzero(
                    Some(value),
                    DEFAULT_MAX_SNIPPETS,
                    MAX_SNIPPETS,
                ));
            }
            if let Some(value) = params
                .get("maximum_number_of_tokens_per_url")
                .and_then(|v| v.as_u64())
            {
                search_params.maximum_number_of_tokens_per_url = Some(clamp_range(
                    Some(value),
                    DEFAULT_MAX_TOKENS_PER_URL,
                    MIN_TOKENS_PER_URL,
                    MAX_TOKENS_PER_URL,
                ));
            }
            if let Some(value) = params
                .get("maximum_number_of_snippets_per_url")
                .and_then(|v| v.as_u64())
            {
                search_params.maximum_number_of_snippets_per_url = Some(clamp_nonzero(
                    Some(value),
                    DEFAULT_MAX_SNIPPETS_PER_URL,
                    MAX_SNIPPETS_PER_URL,
                ));
            }
            if let Some(mode) = params
                .get("context_threshold_mode")
                .and_then(|v| v.as_str())
            {
                search_params.context_threshold_mode = Some(normalize_threshold_mode(Some(mode)));
            }
        }

        search_params
    }
}

#[async_trait]
impl ToolExecutor for WebContextSearchToolExecutor {
    fn name(&self) -> &str {
        WEB_CONTEXT_SEARCH_TOOL_NAME
    }

    fn can_handle(&self, tool_name: &str) -> bool {
        tool_name == WEB_CONTEXT_SEARCH_TOOL_NAME
    }

    async fn execute(
        &self,
        tool_call: &ToolCallInfo,
        context: &ToolExecutionContext<'_>,
    ) -> Result<ToolOutput, ResponseError> {
        let search_params = Self::parse_params(tool_call);
        let started_at = Instant::now();
        let tool_call_id = tool_call.id.as_deref().unwrap_or("unknown");

        tracing::info!(
            tool_name = WEB_CONTEXT_SEARCH_TOOL_NAME,
            tool_call_id,
            model = %context.request.model,
            spellcheck = search_params.spellcheck.unwrap_or(DEFAULT_SPELLCHECK),
            requested_count = search_params.count.unwrap_or(DEFAULT_COUNT),
            requested_max_urls = search_params.maximum_number_of_urls.unwrap_or(DEFAULT_MAX_URLS),
            requested_max_tokens = search_params
                .maximum_number_of_tokens
                .unwrap_or(DEFAULT_MAX_TOKENS),
            requested_max_snippets = search_params
                .maximum_number_of_snippets
                .unwrap_or(DEFAULT_MAX_SNIPPETS),
            requested_max_tokens_per_url = search_params
                .maximum_number_of_tokens_per_url
                .unwrap_or(DEFAULT_MAX_TOKENS_PER_URL),
            requested_max_snippets_per_url = search_params
                .maximum_number_of_snippets_per_url
                .unwrap_or(DEFAULT_MAX_SNIPPETS_PER_URL),
            threshold_mode = search_params
                .context_threshold_mode
                .as_deref()
                .unwrap_or(DEFAULT_THRESHOLD_MODE),
            "Web context search tool started"
        );

        let sources = self
            .provider
            .search_context(search_params)
            .await
            .map_err(|error| {
                let error_category = super::web_search_error_category(&error);
                tracing::warn!(
                    tool_name = WEB_CONTEXT_SEARCH_TOOL_NAME,
                    tool_call_id,
                    model = %context.request.model,
                    error_category,
                    elapsed_ms = started_at.elapsed().as_millis() as u64,
                    "Web context search tool failed"
                );
                ResponseError::InternalError(format!("Web context search failed: {error_category}"))
            })?;

        let (snippet_count, total_snippet_chars) = super::web_search_result_stats(&sources);
        tracing::info!(
            tool_name = WEB_CONTEXT_SEARCH_TOOL_NAME,
            tool_call_id,
            model = %context.request.model,
            result_count = sources.len(),
            snippet_count,
            total_snippet_chars,
            empty_result = sources.is_empty(),
            elapsed_ms = started_at.elapsed().as_millis() as u64,
            "Web context search tool completed"
        );

        Ok(ToolOutput::WebSearch { sources })
    }

    async fn emit_start(
        &self,
        tool_call: &ToolCallInfo,
        event_ctx: &mut ToolEventContext<'_>,
    ) -> Result<(), ResponseError> {
        let item = create_web_context_search_item(
            event_ctx,
            &tool_call.query,
            ResponseItemStatus::InProgress,
        );
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

        let item = create_web_context_search_item(
            event_ctx,
            &tool_call.query,
            ResponseItemStatus::Completed,
        );
        event_ctx.emit_item_done(item).await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses::models::CreateResponseRequest;
    use crate::responses::tools::{WebSearchError, WebSearchResult};
    use std::sync::{Arc as StdArc, Mutex};

    struct MockWebContextSearchProvider {
        results: Vec<WebSearchResult>,
        last_params: StdArc<Mutex<Option<WebContextSearchParams>>>,
    }

    #[async_trait]
    impl WebContextSearchProviderTrait for MockWebContextSearchProvider {
        async fn search_context(
            &self,
            params: WebContextSearchParams,
        ) -> Result<Vec<WebSearchResult>, WebSearchError> {
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
    async fn test_web_context_search_parses_and_clamps_params() {
        let last_params = StdArc::new(Mutex::new(None));
        let provider = Arc::new(MockWebContextSearchProvider {
            results: vec![],
            last_params: last_params.clone(),
        });
        let executor = WebContextSearchToolExecutor::new(provider);

        let tool_call = ToolCallInfo {
            id: Some("call_test".to_string()),
            tool_type: WEB_CONTEXT_SEARCH_TOOL_NAME.to_string(),
            query: "test query".to_string(),
            params: Some(serde_json::json!({
                "country": "US",
                "search_lang": "en",
                "freshness": "pw",
                "spellcheck": false,
                "count": 99,
                "maximum_number_of_urls": 99,
                "maximum_number_of_tokens": 99999,
                "maximum_number_of_snippets": 999,
                "maximum_number_of_tokens_per_url": 99999,
                "maximum_number_of_snippets_per_url": 999,
                "context_threshold_mode": "disabled"
            })),
            thought_signature: None,
        };

        let request = create_test_request();
        let context = ToolExecutionContext { request: &request };

        let result = executor.execute(&tool_call, &context).await;
        assert!(result.is_ok());

        let guard = last_params.lock().expect("lock last_params");
        let params = guard.as_ref().expect("expected captured params");
        assert_eq!(params.country.as_deref(), Some("US"));
        assert_eq!(params.search_lang.as_deref(), Some("en"));
        assert_eq!(params.freshness.as_deref(), Some("pw"));
        assert_eq!(params.spellcheck, Some(false));
        assert_eq!(params.count, Some(MAX_COUNT));
        assert_eq!(params.maximum_number_of_urls, Some(MAX_URLS));
        assert_eq!(params.maximum_number_of_tokens, Some(MAX_TOKENS));
        assert_eq!(params.maximum_number_of_snippets, Some(MAX_SNIPPETS));
        assert_eq!(
            params.maximum_number_of_tokens_per_url,
            Some(MAX_TOKENS_PER_URL)
        );
        assert_eq!(
            params.maximum_number_of_snippets_per_url,
            Some(MAX_SNIPPETS_PER_URL)
        );
        assert_eq!(params.context_threshold_mode.as_deref(), Some("disabled"));
    }

    #[tokio::test]
    async fn test_web_context_search_returns_sources() {
        let provider = Arc::new(MockWebContextSearchProvider {
            results: vec![WebSearchResult {
                title: "Test".to_string(),
                url: "https://example.com".to_string(),
                snippet: "Context snippet".to_string(),
            }],
            last_params: StdArc::new(Mutex::new(None)),
        });
        let executor = WebContextSearchToolExecutor::new(provider);
        let tool_call = ToolCallInfo {
            id: Some("call_test".to_string()),
            tool_type: WEB_CONTEXT_SEARCH_TOOL_NAME.to_string(),
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
                assert_eq!(sources[0].snippet, "Context snippet");
            }
            _ => panic!("Expected WebSearch output"),
        }
    }
}
