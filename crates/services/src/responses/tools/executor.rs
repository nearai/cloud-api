//! Tool Executor Framework
//!
//! This module provides a trait-based abstraction for tool execution,
//! enabling extensible tool handling with a consistent interface.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::responses::errors::ResponseError;
use crate::responses::models::{CreateResponseRequest, ResponseOutputItem, ResponseStreamEvent};
use crate::responses::ports::ResponseItemRepositoryTrait;
use crate::responses::service_helpers::{EventEmitter, ResponseStreamContext, ToolCallInfo};

/// Context for emitting tool-specific events during execution
pub struct ToolEventContext<'a> {
    pub stream_ctx: &'a mut ResponseStreamContext,
    pub emitter: &'a mut EventEmitter,
    pub tool_call_id: &'a str,
    pub response_items_repository: Option<&'a Arc<dyn ResponseItemRepositoryTrait>>,
}

impl<'a> ToolEventContext<'a> {
    /// Emit a simple event with just event_type and optional item_id
    pub async fn emit_simple_event(&mut self, event_type: &str) -> Result<(), ResponseError> {
        let event = ResponseStreamEvent {
            event_type: event_type.to_string(),
            sequence_number: Some(self.stream_ctx.next_sequence()),
            response: None,
            output_index: Some(self.stream_ctx.output_item_index),
            content_index: None,
            item: None,
            item_id: Some(self.tool_call_id.to_string()),
            part: None,
            delta: None,
            text: None,
            logprobs: None,
            obfuscation: None,
            annotation_index: None,
            annotation: None,
            conversation_title: None,
        };
        self.emitter.send_raw(event).await
    }

    /// Emit an item added event
    pub async fn emit_item_added(&mut self, item: ResponseOutputItem) -> Result<(), ResponseError> {
        self.emitter
            .emit_item_added(self.stream_ctx, item, self.tool_call_id.to_string())
            .await
    }

    /// Emit an item done event and optionally store to repository
    pub async fn emit_item_done(&mut self, item: ResponseOutputItem) -> Result<(), ResponseError> {
        self.emitter
            .emit_item_done(self.stream_ctx, item.clone(), self.tool_call_id.to_string())
            .await?;

        // Store response item if repository is provided
        if let Some(repo) = &self.response_items_repository {
            if let Err(e) = repo
                .create(
                    self.stream_ctx.response_id.clone(),
                    self.stream_ctx.api_key_id,
                    self.stream_ctx.conversation_id,
                    item,
                )
                .await
            {
                tracing::warn!("Failed to store response item: {:?}", e);
            }
        }

        self.stream_ctx.next_output_index();
        Ok(())
    }
}

/// Output from tool execution
///
/// Each variant carries the data specific to that tool type.
/// The service layer pattern-matches on this to handle side effects
/// (like updating the citation tracker for search results).
#[derive(Debug, Clone)]
pub enum ToolOutput {
    /// Plain text response (MCP tools, errors, etc.)
    Text(String),

    /// Web search results with structured source data
    WebSearch {
        sources: Vec<super::ports::WebSearchResult>,
    },

    /// File search results with structured data
    FileSearch {
        /// Raw search results
        results: Vec<super::ports::FileSearchResult>,
    },
}

/// Result of tool execution indicating how the agent loop should proceed
#[derive(Debug, Clone)]
pub enum ToolExecutionResult {
    /// Tool executed successfully, continue agent loop
    Success,
    /// Tool requires approval, pause agent loop
    ApprovalRequired,
}

/// Context for tool execution
///
/// Provides read-only access to request data. Tool executors should be
/// stateless - any state management happens in the service layer.
pub struct ToolExecutionContext<'a> {
    /// The original request
    pub request: &'a CreateResponseRequest,
}

/// Trait for tool executors
///
/// Each tool type (web_search, file_search, MCP) implements this trait
/// to provide a consistent interface for tool execution.
///
/// Executors should be stateless - they receive read-only context and
/// return a `ToolOutput` enum that the service layer pattern-matches on.
///
/// Tools can optionally emit events at start and completion by overriding
/// `emit_start` and `emit_complete`. The default implementations are no-ops.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Returns the name of this tool executor (for debugging/logging)
    fn name(&self) -> &str;

    /// Check if this executor can handle the given tool name
    fn can_handle(&self, tool_name: &str) -> bool;

    /// Execute the tool with the given parameters
    ///
    /// # Arguments
    /// * `tool_call` - Information about the tool call including name, query, and params
    /// * `context` - Read-only context with request data
    ///
    /// # Returns
    /// * `Ok(ToolOutput)` - The tool executed successfully with typed output
    /// * `Err(ResponseError)` - The tool execution failed
    async fn execute(
        &self,
        tool_call: &ToolCallInfo,
        context: &ToolExecutionContext<'_>,
    ) -> Result<ToolOutput, ResponseError>;

    /// Emit tool-specific events when execution starts
    ///
    /// Override this method to emit custom events before tool execution.
    /// Default implementation is a no-op.
    async fn emit_start(
        &self,
        _tool_call: &ToolCallInfo,
        _event_ctx: &mut ToolEventContext<'_>,
    ) -> Result<(), ResponseError> {
        Ok(())
    }

    /// Emit tool-specific events when execution completes
    ///
    /// Override this method to emit custom events after tool execution.
    /// Default implementation is a no-op.
    async fn emit_complete(
        &self,
        _tool_call: &ToolCallInfo,
        _event_ctx: &mut ToolEventContext<'_>,
    ) -> Result<(), ResponseError> {
        Ok(())
    }

    /// Handle an error from tool execution.
    ///
    /// This allows each executor to handle its own errors in a custom way.
    /// For example, MCP executor handles `McpApprovalRequired` by emitting
    /// an approval request.
    ///
    /// Returns:
    /// - `Ok(Some(ToolOutput))` to convert error to output (e.g., error message for LLM)
    /// - `Ok(None)` to signal the error was handled but requires special control flow (e.g., approval required)
    /// - `Err(ResponseError)` to propagate the error up
    ///
    /// Default implementation converts the error to a text message for the LLM.
    async fn handle_error(
        &self,
        error: ResponseError,
        _tool_call: &ToolCallInfo,
        _event_ctx: &mut ToolEventContext<'_>,
    ) -> Result<Option<ToolOutput>, ResponseError> {
        // Default: convert error to text output for LLM
        Ok(Some(ToolOutput::Text(format!("ERROR: {error}"))))
    }
}

/// Registry for tool executors
///
/// Holds a collection of tool executors and dispatches tool calls
/// to the appropriate executor based on the tool name.
/// Maximum consecutive failures before logging error instead of warning
pub const MAX_CONSECUTIVE_TOOL_FAILURES: u32 = 3;

pub struct ToolRegistry {
    executors: Vec<Arc<dyn ToolExecutor>>,
    /// Track consecutive failures per tool type for retry-aware logging
    failure_counts: RwLock<HashMap<String, u32>>,
}

impl ToolRegistry {
    /// Create a new empty tool registry
    pub fn new() -> Self {
        Self {
            executors: Vec::new(),
            failure_counts: RwLock::new(HashMap::new()),
        }
    }

    /// Reset the failure counter for a tool type (called on success)
    fn reset_failure_count(&self, tool_type: &str) {
        if let Ok(mut counts) = self.failure_counts.write() {
            counts.remove(tool_type);
        }
    }

    /// Increment failure counter for a tool type and return the new count
    fn increment_failure_count(&self, tool_type: &str) -> u32 {
        if let Ok(mut counts) = self.failure_counts.write() {
            let count = counts.entry(tool_type.to_string()).or_insert(0);
            *count += 1;
            *count
        } else {
            1
        }
    }

    /// Log a tool error with retry-aware messaging
    fn log_tool_error(&self, tool_type: &str, failure_count: u32) {
        if failure_count > MAX_CONSECUTIVE_TOOL_FAILURES {
            tracing::error!(
                tool = %tool_type,
                failures = %failure_count,
                "Tool failed after {} attempts. Error fed back to LLM for correction.",
                MAX_CONSECUTIVE_TOOL_FAILURES,
            );
        } else {
            tracing::warn!(
                tool = %tool_type,
                attempt = %failure_count,
                max_retries = MAX_CONSECUTIVE_TOOL_FAILURES,
                "Tool failed, feeding error back to LLM for retry",
            );
        }
    }

    /// Register a tool executor
    pub fn register(&mut self, executor: Arc<dyn ToolExecutor>) {
        self.executors.push(executor);
    }

    /// Execute a tool call by finding the appropriate executor
    ///
    /// Iterates through registered executors and uses the first one
    /// that can handle the tool name.
    pub async fn execute(
        &self,
        tool_call: &ToolCallInfo,
        context: &ToolExecutionContext<'_>,
    ) -> Result<ToolOutput, ResponseError> {
        // Check for empty tool type
        if tool_call.tool_type.trim().is_empty() {
            return Err(ResponseError::EmptyToolName);
        }

        for executor in &self.executors {
            if executor.can_handle(&tool_call.tool_type) {
                let result = executor.execute(tool_call, context).await;
                // Reset failure count on success
                if result.is_ok() {
                    self.reset_failure_count(&tool_call.tool_type);
                }
                return result;
            }
        }

        Err(ResponseError::UnknownTool(tool_call.tool_type.clone()))
    }

    /// Check if any executor can handle the given tool name
    pub fn can_handle(&self, tool_name: &str) -> bool {
        self.executors.iter().any(|e| e.can_handle(tool_name))
    }

    /// Emit start events for a tool call
    ///
    /// Finds the appropriate executor and calls its emit_start method.
    pub async fn emit_start(
        &self,
        tool_call: &ToolCallInfo,
        event_ctx: &mut ToolEventContext<'_>,
    ) -> Result<(), ResponseError> {
        for executor in &self.executors {
            if executor.can_handle(&tool_call.tool_type) {
                return executor.emit_start(tool_call, event_ctx).await;
            }
        }
        Ok(()) // No-op if no executor found
    }

    /// Emit complete events for a tool call
    ///
    /// Finds the appropriate executor and calls its emit_complete method.
    pub async fn emit_complete(
        &self,
        tool_call: &ToolCallInfo,
        event_ctx: &mut ToolEventContext<'_>,
    ) -> Result<(), ResponseError> {
        for executor in &self.executors {
            if executor.can_handle(&tool_call.tool_type) {
                return executor.emit_complete(tool_call, event_ctx).await;
            }
        }
        Ok(()) // No-op if no executor found
    }

    /// Handle an error from tool execution
    ///
    /// Finds the appropriate executor and calls its handle_error method.
    /// If no executor is found, converts the error to a text message.
    pub async fn handle_error(
        &self,
        error: ResponseError,
        tool_call: &ToolCallInfo,
        event_ctx: &mut ToolEventContext<'_>,
    ) -> Result<Option<ToolOutput>, ResponseError> {
        // Track and log failure
        let failure_count = self.increment_failure_count(&tool_call.tool_type);
        self.log_tool_error(&tool_call.tool_type, failure_count);

        // Delegate to executor for custom handling (e.g., MCP approval)
        for executor in &self.executors {
            if executor.can_handle(&tool_call.tool_type) {
                return executor.handle_error(error, tool_call, event_ctx).await;
            }
        }
        // No executor found, use default behavior
        Ok(Some(ToolOutput::Text(format!("ERROR: {error}"))))
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockToolExecutor {
        name: String,
        handles: Vec<String>,
    }

    #[async_trait]
    impl ToolExecutor for MockToolExecutor {
        fn name(&self) -> &str {
            &self.name
        }

        fn can_handle(&self, tool_name: &str) -> bool {
            self.handles.contains(&tool_name.to_string())
        }

        async fn execute(
            &self,
            _tool_call: &ToolCallInfo,
            _context: &ToolExecutionContext<'_>,
        ) -> Result<ToolOutput, ResponseError> {
            Ok(ToolOutput::Text(format!("Executed by {}", self.name)))
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

    #[test]
    fn test_registry_can_handle() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(MockToolExecutor {
            name: "test".to_string(),
            handles: vec!["web_search".to_string()],
        }));

        assert!(registry.can_handle("web_search"));
        assert!(!registry.can_handle("unknown_tool"));
    }

    #[tokio::test]
    async fn test_registry_execute_unknown_tool() {
        let registry = ToolRegistry::new();
        let tool_call = ToolCallInfo {
            tool_type: "unknown".to_string(),
            query: "test".to_string(),
            params: None,
        };

        let request = create_test_request();
        let context = ToolExecutionContext { request: &request };

        let result = registry.execute(&tool_call, &context).await;
        assert!(matches!(result, Err(ResponseError::UnknownTool(_))));
    }

    #[tokio::test]
    async fn test_registry_execute_empty_tool_name() {
        let registry = ToolRegistry::new();
        let tool_call = ToolCallInfo {
            tool_type: "  ".to_string(),
            query: "test".to_string(),
            params: None,
        };

        let request = create_test_request();
        let context = ToolExecutionContext { request: &request };

        let result = registry.execute(&tool_call, &context).await;
        assert!(matches!(result, Err(ResponseError::EmptyToolName)));
    }

    #[tokio::test]
    async fn test_registry_execute_success() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(MockToolExecutor {
            name: "web_search".to_string(),
            handles: vec!["web_search".to_string()],
        }));

        let tool_call = ToolCallInfo {
            tool_type: "web_search".to_string(),
            query: "test".to_string(),
            params: None,
        };

        let request = create_test_request();
        let context = ToolExecutionContext { request: &request };

        let result = registry.execute(&tool_call, &context).await.unwrap();
        match result {
            ToolOutput::Text(content) => assert_eq!(content, "Executed by web_search"),
            _ => panic!("Expected Text output"),
        }
    }

    #[test]
    fn test_tool_output_variants() {
        // Text variant
        let text_output = ToolOutput::Text("hello".to_string());
        match text_output {
            ToolOutput::Text(s) => assert_eq!(s, "hello"),
            _ => panic!("Expected Text"),
        }

        // WebSearch variant - just sources, no formatted
        let web_output = ToolOutput::WebSearch { sources: vec![] };
        match web_output {
            ToolOutput::WebSearch { sources } => assert!(sources.is_empty()),
            _ => panic!("Expected WebSearch"),
        }

        // FileSearch variant - just results, no formatted
        let file_output = ToolOutput::FileSearch { results: vec![] };
        match file_output {
            ToolOutput::FileSearch { results } => assert!(results.is_empty()),
            _ => panic!("Expected FileSearch"),
        }
    }
}
