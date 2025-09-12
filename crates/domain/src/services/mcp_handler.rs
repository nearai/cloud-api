use super::CompletionHandler;
use crate::{
    errors::CompletionError,
    models::*,
    providers::{StreamChunk, ModelInfo},
    mcp::{McpClientManager, CallToolResult, ContentHelpers},
};
use database::{Database, models::McpConnector};
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use tracing::{debug, error};
use serde_json::json;

/// MCP-aware completion handler that can call external MCP servers for tools
pub struct McpCompletionHandler {
    base_handler: Arc<dyn CompletionHandler>,
    database: Option<Arc<Database>>,
    mcp_manager: Arc<McpClientManager>,
}

impl McpCompletionHandler {
    pub fn new(base_handler: Arc<dyn CompletionHandler>, database: Option<Arc<Database>>) -> Self {
        Self {
            base_handler,
            database,
            mcp_manager: Arc::new(McpClientManager::new()),
        }
    }

    /// Extract tool calls from messages if present
    fn extract_tool_calls(&self, messages: &[ChatMessage]) -> Vec<ToolCall> {
        let mut tool_calls = Vec::new();
        
        for message in messages {
            if let Some(calls) = &message.tool_calls {
                tool_calls.extend(calls.clone());
            }
        }
        
        tool_calls
    }

    /// Process tool calls using MCP connectors
    async fn process_tool_calls(
        &self,
        tool_calls: Vec<ToolCall>,
        organization_id: Option<uuid::Uuid>,
    ) -> Result<Vec<ChatMessage>, CompletionError> {
        let db = match &self.database {
            Some(db) => db,
            None => {
                debug!("No database configured, skipping MCP tool processing");
                return Ok(Vec::new());
            }
        };

        let org_id = match organization_id {
            Some(id) => id,
            None => {
                debug!("No organization ID provided, skipping MCP tool processing");
                return Ok(Vec::new());
            }
        };

        // Get active MCP connectors for the organization
        let connectors = db.mcp_connectors
            .list_active_by_organization(org_id)
            .await
            .map_err(|e| CompletionError::InternalError(format!("Failed to get MCP connectors: {}", e)))?;

        if connectors.is_empty() {
            debug!("No active MCP connectors for organization {}", org_id);
            return Ok(Vec::new());
        }

        let mut tool_results = Vec::new();

        for tool_call in tool_calls {
            let tool_name = &tool_call.function.name;
            let tool_args = tool_call.function.arguments
                .as_ref()
                .and_then(|args| serde_json::from_str::<serde_json::Value>(args).ok());

            // Try each connector until we find one that can handle this tool
            let mut handled = false;
            for connector in &connectors {
                match self.call_mcp_tool(connector.clone(), tool_name.clone(), tool_args.clone()).await {
                    Ok(Some(result)) => {
                        // Convert MCP tool result to chat message
                        let content = self.format_tool_result(&result);
                        tool_results.push(ChatMessage {
                            role: MessageRole::Tool,
                            content: Some(content),
                            name: Some(tool_name.clone()),
                            tool_call_id: Some(tool_call.id.clone()),
                            tool_calls: None,
                        });
                        handled = true;
                        break;
                    }
                    Ok(None) => {
                        // This connector doesn't have this tool, try the next one
                        continue;
                    }
                    Err(e) => {
                        error!("Error calling tool {} on connector {}: {}", tool_name, connector.name, e);
                        continue;
                    }
                }
            }

            if !handled {
                // No connector could handle this tool
                tool_results.push(ChatMessage {
                    role: MessageRole::Tool,
                    content: Some(format!("Error: Tool '{}' not found in any MCP connector", tool_name)),
                    name: Some(tool_name.clone()),
                    tool_call_id: Some(tool_call.id.clone()),
                    tool_calls: None,
                });
            }
        }

        Ok(tool_results)
    }

    /// Call a tool on an MCP connector
    async fn call_mcp_tool(
        &self,
        connector: McpConnector,
        tool_name: String,
        arguments: Option<serde_json::Value>,
    ) -> Result<Option<CallToolResult>, CompletionError> {
        // First check if this connector has the tool
        let tools = self.mcp_manager.list_tools(&connector)
            .await
            .map_err(|e| CompletionError::InternalError(format!("Failed to list tools: {}", e)))?;

        let has_tool = tools.iter().any(|t| t.name == tool_name);
        if !has_tool {
            return Ok(None);
        }

        // Call the tool with retry logic
        let result = self.mcp_manager.call_tool_with_retry(&connector, tool_name.clone(), arguments, 2)
            .await
            .map_err(|e| CompletionError::InternalError(format!("Tool call failed: {}", e)))?;

        // Log usage if database is available
        if let Some(db) = &self.database {
            let _ = db.mcp_connectors.log_usage(
                connector.id,
                uuid::Uuid::new_v4(), // TODO: Get actual user ID from context
                format!("tools/call:{}", tool_name),
                None,
                Some(json!(result)),
                Some(200),
                None,
                None,
            ).await;
        }

        Ok(Some(result))
    }

    /// Format tool result for inclusion in chat context
    fn format_tool_result(&self, result: &CallToolResult) -> String {
        let mut formatted = String::new();
        
        // result.content is Vec<Content>, not Option<Vec<Content>>
        for content in &result.content {
            let text = content.to_string_representation();
            if !formatted.is_empty() {
                formatted.push('\n');
            }
            formatted.push_str(&text);
        }
        
        if result.is_error.unwrap_or(false) {
            format!("Error: {}", formatted)
        } else {
            formatted
        }
    }

    /// Enhance chat messages with tool results
    async fn enhance_messages_with_tools(
        &self,
        mut params: ChatCompletionParams,
        organization_id: Option<uuid::Uuid>,
    ) -> Result<ChatCompletionParams, CompletionError> {
        // Extract any tool calls from the messages
        let tool_calls = self.extract_tool_calls(&params.messages);
        
        if !tool_calls.is_empty() {
            // Process the tool calls
            let tool_results = self.process_tool_calls(tool_calls, organization_id).await?;
            
            // Add tool results to the messages
            params.messages.extend(tool_results);
        }
        
        // Check if we should make available tools from MCP servers
        if params.tools.is_none() && organization_id.is_some() {
            if let Some(tools) = self.get_available_mcp_tools(organization_id.unwrap()).await? {
                params.tools = Some(tools);
            }
        }
        
        Ok(params)
    }

    /// Get available tools from all active MCP connectors for an organization
    async fn get_available_mcp_tools(&self, organization_id: uuid::Uuid) -> Result<Option<Vec<ToolDefinition>>, CompletionError> {
        let db = match &self.database {
            Some(db) => db,
            None => return Ok(None),
        };

        let connectors = db.mcp_connectors
            .list_active_by_organization(organization_id)
            .await
            .map_err(|e| CompletionError::InternalError(format!("Failed to get MCP connectors: {}", e)))?;

        if connectors.is_empty() {
            return Ok(None);
        }

        let mut all_tools = Vec::new();

        for connector in connectors {
            match self.mcp_manager.list_tools(&connector).await {
                Ok(tools) => {
                    for tool in tools {
                        all_tools.push(ToolDefinition {
                            type_: "function".to_string(),
                            function: FunctionDefinition {
                                name: tool.name.to_string(),
                                description: tool.description.as_ref().map(|d| d.to_string()),
                                parameters: serde_json::to_value(&tool.input_schema).unwrap_or(serde_json::json!({})),
                            },
                        });
                    }
                }
                Err(e) => {
                    error!("Failed to list tools from {}: {}", connector.name, e);
                }
            }
        }

        if all_tools.is_empty() {
            Ok(None)
        } else {
            Ok(Some(all_tools))
        }
    }
}

#[async_trait]
impl CompletionHandler for McpCompletionHandler {
    fn name(&self) -> &str {
        self.base_handler.name()
    }
    
    fn supports_model(&self, model_id: &str) -> bool {
        self.base_handler.supports_model(model_id)
    }
    
    async fn get_available_models(&self) -> Result<Vec<ModelInfo>, CompletionError> {
        self.base_handler.get_available_models().await
    }
    
    async fn chat_completion(&self, params: ChatCompletionParams) -> Result<ChatCompletionResult, CompletionError> {
        // TODO: Get organization_id from context
        let organization_id = None;
        
        // Enhance messages with MCP tool calls if needed
        let enhanced_params = self.enhance_messages_with_tools(params, organization_id).await?;
        
        // Call the base handler with enhanced parameters
        self.base_handler.chat_completion(enhanced_params).await
    }
    
    async fn chat_completion_stream(&self, params: ChatCompletionParams) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError> {
        // TODO: Get organization_id from context
        let organization_id = None;
        
        // Enhance messages with MCP tool calls if needed
        let enhanced_params = self.enhance_messages_with_tools(params, organization_id).await?;
        
        // Call the base handler with enhanced parameters
        self.base_handler.chat_completion_stream(enhanced_params).await
    }
    
    async fn text_completion(&self, params: CompletionParams) -> Result<CompletionResult, CompletionError> {
        // Text completions don't support tools, so just pass through
        self.base_handler.text_completion(params).await
    }
    
    async fn text_completion_stream(&self, params: CompletionParams) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, CompletionError>> + Send>>, CompletionError> {
        // Text completions don't support tools, so just pass through
        self.base_handler.text_completion_stream(params).await
    }
    
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
