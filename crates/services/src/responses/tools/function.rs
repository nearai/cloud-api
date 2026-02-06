//! Function Tool Executor for External/Custom Functions
//!
//! This module handles external function tools defined by clients.
//! Unlike built-in tools (web_search, file_search) or MCP tools,
//! external functions are executed by the client, not the server.
//!
//! Flow:
//! 1. Client defines functions in `tools` array with type: "function"
//! 2. LLM calls the function with arguments
//! 3. Server returns response with status=incomplete, reason="function_call_required"
//! 4. Client executes the function externally
//! 5. Client submits function output via FunctionCallOutput input
//! 6. Server resumes response with the function result in context

use async_trait::async_trait;
use std::collections::HashSet;

use crate::responses::errors::ResponseError;
use crate::responses::models::{CreateResponseRequest, ResponseOutputItem, ResponseTool};
use crate::responses::service_helpers::ToolCallInfo;

use super::executor::{ToolEventContext, ToolExecutionContext, ToolExecutor, ToolOutput};

/// Executor for external function tools
///
/// This executor identifies function tools defined in the request and returns
/// `FunctionCallRequired` errors for them, signaling that the client must
/// execute the function and provide the result.
pub struct FunctionToolExecutor {
    /// Names of external functions defined in the request
    function_names: HashSet<String>,
}

impl FunctionToolExecutor {
    /// Create a new FunctionToolExecutor from a request
    ///
    /// Extracts all client-executed tool names from the request's tools array:
    /// - Custom function tools (ResponseTool::Function)
    /// - CodeInterpreter tool (client must execute, no server implementation)
    /// - Computer tool (client must execute, no server implementation)
    pub fn new(request: &CreateResponseRequest) -> Self {
        let mut function_names = HashSet::new();

        if let Some(tools) = &request.tools {
            for tool in tools {
                match tool {
                    ResponseTool::Function { name, .. } => {
                        function_names.insert(name.clone());
                    }
                    ResponseTool::CodeInterpreter {} => {
                        function_names.insert(super::CODE_INTERPRETER_TOOL_NAME.to_string());
                    }
                    ResponseTool::Computer {} => {
                        function_names.insert(super::COMPUTER_TOOL_NAME.to_string());
                    }
                    // Other tools are server-executed
                    _ => {}
                }
            }
        }

        Self { function_names }
    }

    /// Check if this executor has any functions registered
    pub fn is_empty(&self) -> bool {
        self.function_names.is_empty()
    }

    /// Get the set of function names
    pub fn function_names(&self) -> &HashSet<String> {
        &self.function_names
    }
}

#[async_trait]
impl ToolExecutor for FunctionToolExecutor {
    fn name(&self) -> &str {
        "function"
    }

    fn can_handle(&self, tool_name: &str) -> bool {
        self.function_names.contains(tool_name)
    }

    async fn execute(
        &self,
        tool_call: &ToolCallInfo,
        _context: &ToolExecutionContext<'_>,
    ) -> Result<ToolOutput, ResponseError> {
        // External functions are never executed by us - always return FunctionCallRequired
        // The service layer will handle this error and create the FunctionCall output item
        Err(ResponseError::FunctionCallRequired {
            name: tool_call.tool_type.clone(),
            call_id: tool_call.id.clone().unwrap_or_default(),
        })
    }

    async fn handle_error(
        &self,
        error: ResponseError,
        tool_call: &ToolCallInfo,
        event_ctx: &mut ToolEventContext<'_>,
    ) -> Result<Option<ToolOutput>, ResponseError> {
        match error {
            ResponseError::FunctionCallRequired { name, call_id } => {
                // External function call - emit FunctionCall item and signal pause
                let arguments = tool_call
                    .params
                    .as_ref()
                    .map(|p| serde_json::to_string(p).unwrap_or_default())
                    .unwrap_or_default();

                // Generate a unique ID for the function call item
                let fc_id = format!(
                    "{}{}",
                    crate::id_prefixes::PREFIX_FC,
                    uuid::Uuid::new_v4().simple()
                );

                let function_call = ResponseOutputItem::FunctionCall {
                    id: fc_id.clone(),
                    response_id: event_ctx.stream_ctx.response_id_str.clone(),
                    previous_response_id: event_ctx.stream_ctx.previous_response_id.clone(),
                    next_response_ids: vec![],
                    created_at: event_ctx.stream_ctx.created_at,
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments,
                    status: "in_progress".to_string(),
                    model: event_ctx.stream_ctx.model.clone(),
                };

                // Store the function call in the database
                if let Some(repo) = &event_ctx.response_items_repository {
                    if let Err(e) = repo
                        .create(
                            event_ctx.stream_ctx.response_id.clone(),
                            event_ctx.stream_ctx.api_key_id,
                            event_ctx.stream_ctx.conversation_id,
                            function_call.clone(),
                        )
                        .await
                    {
                        tracing::warn!("Failed to store function call: {}", e);
                    }
                }

                // Emit function call event
                if let Err(e) = event_ctx.emit_item_added(function_call).await {
                    tracing::debug!("Failed to emit function call event: {}", e);
                }

                // Return None to signal that we need to pause and wait for client
                Ok(None)
            }
            // For other errors, convert to text output for LLM
            other => Ok(Some(ToolOutput::Text(format!("ERROR: {other}")))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_request_with_functions(function_names: Vec<&str>) -> CreateResponseRequest {
        let tools: Vec<ResponseTool> = function_names
            .into_iter()
            .map(|name| ResponseTool::Function {
                name: name.to_string(),
                description: Some(format!("Test function: {}", name)),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {}
                })),
            })
            .collect();

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
            tools: Some(tools),
            tool_choice: None,
            parallel_tool_calls: None,
            reasoning: None,
            include: None,
            metadata: None,
            safety_identifier: None,
            prompt_cache_key: None,
        }
    }

    fn create_test_request_no_tools() -> CreateResponseRequest {
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
    fn test_function_executor_extracts_names() {
        let request = create_test_request_with_functions(vec!["get_weather", "search_database"]);
        let executor = FunctionToolExecutor::new(&request);

        assert!(!executor.is_empty());
        assert!(executor.function_names().contains("get_weather"));
        assert!(executor.function_names().contains("search_database"));
        assert!(!executor.function_names().contains("unknown_function"));
    }

    #[test]
    fn test_function_executor_empty_when_no_tools() {
        let request = create_test_request_no_tools();
        let executor = FunctionToolExecutor::new(&request);

        assert!(executor.is_empty());
    }

    #[test]
    fn test_function_executor_can_handle() {
        let request = create_test_request_with_functions(vec!["get_weather"]);
        let executor = FunctionToolExecutor::new(&request);

        assert!(executor.can_handle("get_weather"));
        assert!(!executor.can_handle("unknown_function"));
        assert!(!executor.can_handle("web_search")); // Built-in tool
    }

    #[tokio::test]
    async fn test_function_executor_returns_required_error() {
        let request = create_test_request_with_functions(vec!["get_weather"]);
        let executor = FunctionToolExecutor::new(&request);

        let tool_call = ToolCallInfo {
            id: Some("call_abc123".to_string()),
            tool_type: "get_weather".to_string(),
            query: "".to_string(),
            params: Some(serde_json::json!({"location": "NYC"})),
            thought_signature: None,
        };

        let context = ToolExecutionContext { request: &request };
        let result = executor.execute(&tool_call, &context).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            ResponseError::FunctionCallRequired { name, call_id } => {
                assert_eq!(name, "get_weather");
                assert_eq!(call_id, "call_abc123");
            }
            other => panic!("Expected FunctionCallRequired, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_function_executor_handles_missing_call_id() {
        let request = create_test_request_with_functions(vec!["get_weather"]);
        let executor = FunctionToolExecutor::new(&request);

        let tool_call = ToolCallInfo {
            id: None, // No call_id
            tool_type: "get_weather".to_string(),
            query: "".to_string(),
            params: None,
            thought_signature: None,
        };

        let context = ToolExecutionContext { request: &request };
        let result = executor.execute(&tool_call, &context).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            ResponseError::FunctionCallRequired { name, call_id } => {
                assert_eq!(name, "get_weather");
                assert_eq!(call_id, ""); // Should be empty string, not None
            }
            other => panic!("Expected FunctionCallRequired, got: {:?}", other),
        }
    }

    fn create_test_request_with_code_interpreter() -> CreateResponseRequest {
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
            tools: Some(vec![ResponseTool::CodeInterpreter {}]),
            tool_choice: None,
            parallel_tool_calls: None,
            reasoning: None,
            include: None,
            metadata: None,
            safety_identifier: None,
            prompt_cache_key: None,
        }
    }

    fn create_test_request_with_computer() -> CreateResponseRequest {
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
            tools: Some(vec![ResponseTool::Computer {}]),
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
    fn test_function_executor_handles_code_interpreter() {
        let request = create_test_request_with_code_interpreter();
        let executor = FunctionToolExecutor::new(&request);

        // Should include code_interpreter as a client-executed tool
        assert!(!executor.is_empty());
        assert!(executor.can_handle("code_interpreter"));
        assert!(!executor.can_handle("web_search")); // Built-in server-executed tool
    }

    #[test]
    fn test_function_executor_handles_computer() {
        let request = create_test_request_with_computer();
        let executor = FunctionToolExecutor::new(&request);

        // Should include computer as a client-executed tool
        assert!(!executor.is_empty());
        assert!(executor.can_handle("computer"));
        assert!(!executor.can_handle("web_search")); // Built-in server-executed tool
    }

    #[tokio::test]
    async fn test_code_interpreter_returns_function_call_required() {
        let request = create_test_request_with_code_interpreter();
        let executor = FunctionToolExecutor::new(&request);

        let tool_call = ToolCallInfo {
            id: Some("call_code_123".to_string()),
            tool_type: "code_interpreter".to_string(),
            query: "".to_string(),
            params: Some(serde_json::json!({"code": "print('Hello')"})),
            thought_signature: None,
        };

        let context = ToolExecutionContext { request: &request };
        let result = executor.execute(&tool_call, &context).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            ResponseError::FunctionCallRequired { name, call_id } => {
                assert_eq!(name, "code_interpreter");
                assert_eq!(call_id, "call_code_123");
            }
            other => panic!("Expected FunctionCallRequired, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_computer_returns_function_call_required() {
        let request = create_test_request_with_computer();
        let executor = FunctionToolExecutor::new(&request);

        let tool_call = ToolCallInfo {
            id: Some("call_computer_456".to_string()),
            tool_type: "computer".to_string(),
            query: "".to_string(),
            params: Some(serde_json::json!({"action": "click", "x": 100, "y": 200})),
            thought_signature: None,
        };

        let context = ToolExecutionContext { request: &request };
        let result = executor.execute(&tool_call, &context).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            ResponseError::FunctionCallRequired { name, call_id } => {
                assert_eq!(name, "computer");
                assert_eq!(call_id, "call_computer_456");
            }
            other => panic!("Expected FunctionCallRequired, got: {:?}", other),
        }
    }
}
