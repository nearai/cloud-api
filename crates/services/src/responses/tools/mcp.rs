//! MCP (Model Context Protocol) Tool Executor for Remote MCP Servers
//!
//! This module provides the `McpToolExecutor` for connecting to remote MCP servers,
//! discovering tools, and executing them during response generation.
//!
//! Reference: https://platform.openai.com/docs/guides/tools-connectors-mcp

use crate::responses::errors::ResponseError;
use crate::responses::models::{
    self, McpApprovalRequirement, McpDiscoveredTool, ResponseOutputItem, ResponseTool,
};
use crate::responses::ports::ResponseItemRepositoryTrait;
use crate::responses::service_helpers::{EventEmitter, ResponseStreamContext, ToolCallInfo};

use super::executor::{ToolEventContext, ToolExecutionContext, ToolExecutor, ToolOutput};

use async_trait::async_trait;
use inference_providers::{FunctionDefinition, ToolDefinition};
use rmcp::{
    model::{CallToolRequestParam, CallToolResult},
    service::{RoleClient, RunningService},
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    },
    ServiceExt,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};
use tracing::debug;

#[cfg(any(test, feature = "test-mocks"))]
use mockall::automock;

// ============================================
// Constants
// ============================================

/// Maximum number of MCP servers allowed per request
pub const MAX_MCP_SERVERS_PER_REQUEST: usize = 5;

/// Maximum number of tools allowed per MCP server
pub const MAX_TOOLS_PER_SERVER: usize = 50;

/// Timeout for connecting to an MCP server (seconds)
pub const CONNECTION_TIMEOUT_SECS: u64 = 30;

/// Timeout for executing a tool on an MCP server (seconds)
pub const TOOL_EXECUTION_TIMEOUT_SECS: u64 = 60;

// ============================================
// MCP Client Trait (mockable)
// ============================================

#[cfg_attr(any(test, feature = "test-mocks"), automock)]
#[async_trait]
pub trait McpClient: Send + Sync {
    async fn list_tools(&self) -> Result<Vec<McpDiscoveredTool>, ResponseError>;

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<String, ResponseError>;
}

#[cfg_attr(any(test, feature = "test-mocks"), automock)]
#[async_trait]
pub trait McpClientFactory: Send + Sync {
    /// Create a new MCP client connection
    async fn create_client(
        &self,
        server_url: &str,
        authorization: Option<String>,
    ) -> Result<Box<dyn McpClient>, ResponseError>;
}

// ============================================
// Real MCP Client Implementation
// ============================================

pub struct RealMcpClient {
    client: Arc<Mutex<RunningService<RoleClient, ()>>>,
}

#[async_trait]
impl McpClient for RealMcpClient {
    async fn list_tools(&self) -> Result<Vec<McpDiscoveredTool>, ResponseError> {
        let client = self.client.lock().await;
        let tools = timeout(
            Duration::from_secs(CONNECTION_TIMEOUT_SECS),
            client.list_all_tools(),
        )
        .await
        .map_err(|_| ResponseError::McpToolDiscoveryFailed("Timeout listing tools".to_string()))?
        .map_err(|e| ResponseError::McpToolDiscoveryFailed(e.to_string()))?;

        Ok(tools
            .into_iter()
            .map(|t| McpDiscoveredTool {
                name: t.name.to_string(),
                description: t.description.map(|s| s.to_string()),
                input_schema: Some(serde_json::Value::Object(t.input_schema.as_ref().clone())),
                annotations: None,
            })
            .collect())
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<String, ResponseError> {
        let args = arguments.as_object().cloned();
        let request = CallToolRequestParam {
            name: tool_name.to_string().into(),
            arguments: args,
        };

        let client = self.client.lock().await;
        let result = timeout(
            Duration::from_secs(TOOL_EXECUTION_TIMEOUT_SECS),
            client.call_tool(request),
        )
        .await
        .map_err(|_| {
            ResponseError::McpToolExecutionFailed(format!(
                "Timeout after {}s",
                TOOL_EXECUTION_TIMEOUT_SECS
            ))
        })?
        .map_err(|e| ResponseError::McpToolExecutionFailed(e.to_string()))?;

        Self::extract_tool_result(&result)
    }
}

impl RealMcpClient {
    /// Extract text content from tool result
    /// Uses JSON serialization to handle the opaque Content type
    fn extract_tool_result(result: &CallToolResult) -> Result<String, ResponseError> {
        let mut texts = Vec::new();

        for content in &result.content {
            // Content is an opaque type - extract text via JSON serialization
            if let Ok(value) = serde_json::to_value(content) {
                if let Some(text) = value.get("text").and_then(|v| v.as_str()) {
                    texts.push(text.to_string());
                }
            }
        }

        let output = texts.join("\n");

        // Check if the tool reported an error
        if result.is_error.unwrap_or(false) {
            return Err(ResponseError::McpToolExecutionFailed(output));
        }

        Ok(output)
    }
}

#[derive(Default)]
pub struct RealMcpClientFactory;

#[async_trait]
impl McpClientFactory for RealMcpClientFactory {
    async fn create_client(
        &self,
        server_url: &str,
        authorization: Option<String>,
    ) -> Result<Box<dyn McpClient>, ResponseError> {
        let mut config = StreamableHttpClientTransportConfig::with_uri(server_url);

        if let Some(auth_header) = &authorization {
            config = config.auth_header(auth_header);
        }

        let transport = StreamableHttpClientTransport::from_config(config);

        let client = timeout(
            Duration::from_secs(CONNECTION_TIMEOUT_SECS),
            ().serve(transport),
        )
        .await
        .map_err(|_| {
            ResponseError::McpConnectionFailed(format!(
                "Initialization timeout after {}s",
                CONNECTION_TIMEOUT_SECS
            ))
        })?
        .map_err(|e| ResponseError::McpConnectionFailed(e.to_string()))?;

        Ok(Box::new(RealMcpClient {
            client: Arc::new(Mutex::new(client)),
        }))
    }
}

// ============================================
// Arc Wrapper for McpClientFactory
// ============================================

/// Wrapper to use Arc<dyn McpClientFactory> as Box<dyn McpClientFactory>
struct ArcClientFactoryWrapper(Arc<dyn McpClientFactory>);

#[async_trait]
impl McpClientFactory for ArcClientFactoryWrapper {
    async fn create_client(
        &self,
        server_url: &str,
        authorization: Option<String>,
    ) -> Result<Box<dyn McpClient>, ResponseError> {
        self.0.create_client(server_url, authorization).await
    }
}

// ============================================
// MCP Server Connection
// ============================================

/// Connection to a single MCP server
struct McpServerConnection {
    /// The MCP client
    client: Box<dyn McpClient>,
    /// Server label for mcp_call output items (used in Phase 2)
    #[allow(dead_code)]
    server_label: String,
    tools: Vec<McpDiscoveredTool>,
    require_approval: McpApprovalRequirement,
}

// ============================================
// MCP Tool Executor
// ============================================

/// Executor for MCP tools within a single response request
///
/// This executor manages connections to remote MCP servers, discovers their tools,
/// and executes tool calls. It is designed to be created per-request and cleaned
/// up when the request completes.
pub struct McpToolExecutor {
    client_factory: Box<dyn McpClientFactory>,
    connections: HashMap<String, McpServerConnection>,
    tool_to_server: HashMap<String, String>,
    /// MCP list tools items (emitted but not stored in DB)
    mcp_list_tools_items: Vec<ResponseOutputItem>,
}

impl Drop for McpToolExecutor {
    fn drop(&mut self) {
        if !self.connections.is_empty() {
            debug!(
                connection_count = self.connections.len(),
                "Cleaning up MCP connections"
            );
            self.connections.clear();
        }
    }
}

impl McpToolExecutor {
    /// Create a new MCP tool executor
    pub fn new() -> Self {
        Self {
            client_factory: Box::new(RealMcpClientFactory),
            connections: HashMap::new(),
            tool_to_server: HashMap::new(),
            mcp_list_tools_items: Vec::new(),
        }
    }

    pub fn with_client_factory(client_factory: Box<dyn McpClientFactory>) -> Self {
        Self {
            client_factory,
            connections: HashMap::new(),
            tool_to_server: HashMap::new(),
            mcp_list_tools_items: Vec::new(),
        }
    }

    /// Create executor with an Arc-wrapped client factory
    pub fn with_arc_client_factory(client_factory: Arc<dyn McpClientFactory>) -> Self {
        Self {
            client_factory: Box::new(ArcClientFactoryWrapper(client_factory)),
            connections: HashMap::new(),
            tool_to_server: HashMap::new(),
            mcp_list_tools_items: Vec::new(),
        }
    }

    /// Connect to MCP servers and discover tools.
    /// For servers with cached tools, skips the list_tools call.
    /// Returns McpListTools items for servers that were freshly discovered.
    pub async fn connect_servers(
        &mut self,
        mcp_tools: Vec<&ResponseTool>,
        cached_tools: &std::collections::HashMap<String, Vec<McpDiscoveredTool>>,
    ) -> Result<Vec<ResponseOutputItem>, ResponseError> {
        if mcp_tools.len() > MAX_MCP_SERVERS_PER_REQUEST {
            return Err(ResponseError::McpServerLimitExceeded {
                max: MAX_MCP_SERVERS_PER_REQUEST,
            });
        }

        let mut output_items = Vec::new();

        for tool in mcp_tools {
            if let ResponseTool::Mcp {
                server_label,
                server_url,
                authorization,
                require_approval,
                allowed_tools,
                ..
            } = tool
            {
                Self::validate_server_url(server_url)?;

                if let Some(cached) = cached_tools.get(server_label) {
                    debug!(
                        server_label = %server_label,
                        tool_count = cached.len(),
                        "Using cached MCP tools (skipping list_tools call)"
                    );

                    for tool in cached {
                        let fq_name = format!("{}:{}", server_label, tool.name);
                        self.tool_to_server.insert(fq_name, server_label.clone());
                    }

                    let client = self
                        .client_factory
                        .create_client(server_url, authorization.clone())
                        .await?;

                    self.connections.insert(
                        server_label.clone(),
                        McpServerConnection {
                            client,
                            server_label: server_label.clone(),
                            tools: cached.clone(),
                            require_approval: require_approval.clone(),
                        },
                    );

                    continue;
                }

                debug!(
                    server_label = %server_label,
                    "Connecting to MCP server (no cache)"
                );

                let client = self
                    .client_factory
                    .create_client(server_url, authorization.clone())
                    .await?;

                let all_tools = client.list_tools().await?;
                debug!(
                    server_label = %server_label,
                    tool_count = all_tools.len(),
                    "Discovered tools from MCP server"
                );

                let tools: Vec<McpDiscoveredTool> = if let Some(allowed) = allowed_tools {
                    all_tools
                        .into_iter()
                        .filter(|t| allowed.contains(&t.name))
                        .collect()
                } else {
                    all_tools
                };

                if tools.len() > MAX_TOOLS_PER_SERVER {
                    return Err(ResponseError::McpToolLimitExceeded {
                        server: server_label.clone(),
                        count: tools.len(),
                        max: MAX_TOOLS_PER_SERVER,
                    });
                }

                for tool in &tools {
                    let fq_name = format!("{}:{}", server_label, tool.name);
                    self.tool_to_server.insert(fq_name, server_label.clone());
                }

                let list_tools_id = format!("mcpl_{}", uuid::Uuid::new_v4().simple());
                output_items.push(ResponseOutputItem::McpListTools {
                    id: list_tools_id,
                    server_label: server_label.clone(),
                    tools: tools.clone(),
                    error: None,
                });

                self.connections.insert(
                    server_label.clone(),
                    McpServerConnection {
                        client,
                        server_label: server_label.clone(),
                        tools,
                        require_approval: require_approval.clone(),
                    },
                );
            }
        }

        self.mcp_list_tools_items = output_items.clone();

        Ok(output_items)
    }

    /// Validate server URL for security
    ///
    /// # Security Requirements
    /// - Must use HTTPS (HTTP not allowed)
    /// - Must not be a private/internal IP address
    pub fn validate_server_url(url: &str) -> Result<(), ResponseError> {
        let parsed = url::Url::parse(url)
            .map_err(|e| ResponseError::McpConnectionFailed(format!("Invalid URL: {}", e)))?;

        // Require HTTPS
        if parsed.scheme() != "https" {
            return Err(ResponseError::McpInsecureUrl);
        }

        // Block private IPs
        if let Some(host) = parsed.host_str() {
            if Self::is_private_host(host) {
                return Err(ResponseError::McpPrivateIpBlocked);
            }
        }

        Ok(())
    }

    /// Check if host is a private/internal address
    fn is_private_host(host: &str) -> bool {
        // Block localhost variants
        if host == "localhost"
            || host == "127.0.0.1"
            || host == "::1"
            || host.ends_with(".localhost")
        {
            return true;
        }

        // Try to parse as IP address
        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            match ip {
                std::net::IpAddr::V4(ipv4) => {
                    ipv4.is_private()
                        || ipv4.is_loopback()
                        || ipv4.is_link_local()
                        || ipv4.is_broadcast()
                        || ipv4.is_unspecified()
                }
                std::net::IpAddr::V6(ipv6) => {
                    ipv6.is_loopback() || ipv6.is_unspecified() || ipv6.is_unique_local()
                }
            }
        } else {
            false
        }
    }

    /// Check if a tool name is an MCP tool (format: "server_label:tool_name")
    pub fn is_mcp_tool(&self, tool_name: &str) -> bool {
        self.tool_to_server.contains_key(tool_name)
    }

    /// Parse MCP tool name into (server_label, tool_name)
    pub fn parse_tool_name(tool_name: &str) -> Option<(&str, &str)> {
        tool_name.split_once(':')
    }

    /// Check if tool requires approval
    pub fn requires_approval(&self, server_label: &str, tool_name: &str) -> bool {
        if let Some(conn) = self.connections.get(server_label) {
            conn.require_approval.requires_approval(tool_name)
        } else {
            // Default to requiring approval if server not found
            true
        }
    }

    /// Execute a tool on an MCP server
    pub async fn execute_tool(
        &self,
        server_label: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<String, ResponseError> {
        let conn = self.connections.get(server_label).ok_or_else(|| {
            ResponseError::McpConnectionFailed(format!("Server '{}' not connected", server_label))
        })?;

        debug!(
            server_label = %server_label,
            tool_name = %tool_name,
            "Executing MCP tool"
        );

        conn.client.call_tool(tool_name, arguments).await
    }

    /// Get all tool definitions for the inference provider
    pub fn get_tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = Vec::new();

        for (server_label, conn) in &self.connections {
            for tool in &conn.tools {
                definitions.push(ToolDefinition {
                    type_: "function".to_string(),
                    function: FunctionDefinition {
                        name: format!("{}:{}", server_label, tool.name),
                        description: Some(tool.description.clone().unwrap_or_default()),
                        parameters: tool.input_schema.clone().unwrap_or(serde_json::json!({
                            "type": "object",
                            "properties": {}
                        })),
                    },
                });
            }
        }

        definitions
    }

    /// Get MCP list tools items (for inclusion in final response)
    pub fn get_mcp_list_tools_items(&self) -> &[ResponseOutputItem] {
        &self.mcp_list_tools_items
    }

    /// Get the list of connected server labels
    pub fn connected_servers(&self) -> Vec<&str> {
        self.connections.keys().map(|s| s.as_str()).collect()
    }

    /// Get tools for a specific server
    pub fn get_server_tools(&self, server_label: &str) -> Option<&[McpDiscoveredTool]> {
        self.connections
            .get(server_label)
            .map(|c| c.tools.as_slice())
    }

    // ============================================
    // Approval Response Processing
    // ============================================

    /// Process an MCP approval response from the client.
    ///
    /// Validates that the approval request exists in the previous response,
    /// then either executes the approved tool or returns a rejection message.
    ///
    /// Returns `Ok(Some(message))` with the tool result or rejection message,
    /// or `Ok(None)` if no action needed.
    pub async fn process_approval_response(
        &self,
        approval_request_id: &str,
        approve: bool,
        previous_response_items: &[ResponseOutputItem],
    ) -> Result<Option<String>, ResponseError> {
        // Find the matching approval request in the previous response
        let approval_request = previous_response_items
            .iter()
            .find_map(|item| {
                if let ResponseOutputItem::McpApprovalRequest {
                    id,
                    server_label,
                    name,
                    arguments,
                    ..
                } = item
                {
                    if id == approval_request_id {
                        Some((server_label.clone(), name.clone(), arguments.clone()))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                ResponseError::McpApprovalRequestNotFound(approval_request_id.to_string())
            })?;

        let (server_label, tool_name, arguments_str) = approval_request;

        if approve {
            // Parse arguments and execute the tool
            let arguments: serde_json::Value =
                serde_json::from_str(&arguments_str).unwrap_or_else(|_| serde_json::json!({}));

            let result = self
                .execute_tool(&server_label, &tool_name, arguments)
                .await?;

            Ok(Some(result))
        } else {
            // Return rejection message for LLM context
            Ok(Some(format!(
                "Tool call '{}' on server '{}' was rejected by the user.",
                tool_name, server_label
            )))
        }
    }
}

impl Default for McpToolExecutor {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================
// ToolExecutor Implementation
// ============================================

#[async_trait]
impl ToolExecutor for McpToolExecutor {
    fn name(&self) -> &str {
        "mcp"
    }

    fn can_handle(&self, tool_name: &str) -> bool {
        // MCP tools have format "server_label:tool_name"
        self.is_mcp_tool(tool_name)
    }

    async fn execute(
        &self,
        tool_call: &ToolCallInfo,
        _context: &ToolExecutionContext<'_>,
    ) -> Result<ToolOutput, ResponseError> {
        let tool_name = &tool_call.tool_type;

        let (server_label, mcp_tool_name) = Self::parse_tool_name(tool_name)
            .ok_or_else(|| ResponseError::UnknownTool(tool_name.clone()))?;

        // Parse arguments from tool call
        let arguments = tool_call
            .params
            .clone()
            .unwrap_or_else(|| serde_json::json!({}));

        // Check if tool requires approval
        if self.requires_approval(server_label, mcp_tool_name) {
            return Err(ResponseError::McpApprovalRequired {
                server: server_label.to_string(),
                tool: mcp_tool_name.to_string(),
            });
        }

        // Execute the MCP tool
        let result = self
            .execute_tool(server_label, mcp_tool_name, arguments)
            .await?;

        Ok(ToolOutput::Text(result))
    }

    async fn handle_error(
        &self,
        error: ResponseError,
        tool_call: &ToolCallInfo,
        event_ctx: &mut ToolEventContext<'_>,
    ) -> Result<Option<ToolOutput>, ResponseError> {
        match error {
            ResponseError::McpApprovalRequired { .. } => {
                // MCP tool requires approval - emit approval request
                let tool_name = &tool_call.tool_type;

                let (server_label, mcp_tool_name) = match Self::parse_tool_name(tool_name) {
                    Some(parsed) => parsed,
                    None => {
                        return Ok(Some(ToolOutput::Text(format!(
                            "ERROR: Invalid MCP tool name: {tool_name}"
                        ))))
                    }
                };

                // Parse arguments from tool call
                let arguments = tool_call
                    .params
                    .clone()
                    .unwrap_or_else(|| serde_json::json!({}));

                // Create approval request
                let approval_id = format!("mcpr_{}", uuid::Uuid::new_v4().simple());
                let approval_request = ResponseOutputItem::McpApprovalRequest {
                    id: approval_id.clone(),
                    response_id: event_ctx.stream_ctx.response_id_str.clone(),
                    previous_response_id: event_ctx.stream_ctx.previous_response_id.clone(),
                    next_response_ids: vec![],
                    created_at: event_ctx.stream_ctx.created_at,
                    server_label: server_label.to_string(),
                    name: mcp_tool_name.to_string(),
                    arguments: serde_json::to_string(&arguments).unwrap_or_default(),
                    model: event_ctx.stream_ctx.model.clone(),
                };

                // Store approval request in database
                if let Some(repo) = &event_ctx.response_items_repository {
                    if let Err(e) = repo
                        .create(
                            event_ctx.stream_ctx.response_id.clone(),
                            event_ctx.stream_ctx.api_key_id,
                            event_ctx.stream_ctx.conversation_id,
                            approval_request.clone(),
                        )
                        .await
                    {
                        tracing::warn!("Failed to store MCP approval request: {}", e);
                    }
                }

                // Emit approval request event
                if let Err(e) = event_ctx.emit_item_added(approval_request).await {
                    tracing::debug!("Failed to emit MCP approval request event: {}", e);
                }

                // Return None to signal approval is required (special control flow)
                Ok(None)
            }
            // For other errors, convert to text output for LLM
            other => Ok(Some(ToolOutput::Text(format!("ERROR: {other}")))),
        }
    }
}

// ============================================
// Standalone MCP Helper Functions
// ============================================

/// Result of MCP setup containing the executor, tool definitions, and any approval messages.
pub struct McpSetupResult {
    /// The MCP executor to register with the tool registry
    pub executor: Arc<McpToolExecutor>,
    /// Tool definitions to add to the LLM request
    pub tool_definitions: Vec<ToolDefinition>,
    /// Messages from processed approval responses (to add to conversation context)
    pub approval_messages: Vec<crate::completions::ports::CompletionMessage>,
}

/// Set up MCP for the request: connect to servers, discover tools, and process approvals.
///
/// This is the main entry point for MCP setup in service.rs. It:
/// 1. Extracts cached tools from the request input
/// 2. Connects to MCP servers and discovers tools
/// 3. Emits mcp_list_tools items for client-side caching
/// 4. Processes any approval responses from the input
///
/// Returns None if no MCP tools are configured in the request.
pub async fn setup_mcp(
    request: &models::CreateResponseRequest,
    client_factory: Option<&Arc<dyn McpClientFactory>>,
    response_items_repository: &Arc<dyn ResponseItemRepositoryTrait>,
    ctx: &mut ResponseStreamContext,
    emitter: &mut EventEmitter,
) -> Result<Option<McpSetupResult>, ResponseError> {
    // Check if there are any MCP tools in the request
    let request_tools = match &request.tools {
        Some(tools) => tools,
        None => return Ok(None),
    };

    let mcp_tools: Vec<&ResponseTool> = request_tools
        .iter()
        .filter(|t| matches!(t, ResponseTool::Mcp { .. }))
        .collect();

    if mcp_tools.is_empty() {
        return Ok(None);
    }

    // Extract cached tools from input
    let cached_tools = extract_cached_mcp_tools(request);

    // Initialize the MCP executor
    let (mcp_executor, tool_definitions) =
        match initialize_mcp_executor(request_tools, &cached_tools, client_factory, ctx, emitter)
            .await?
        {
            Some(result) => result,
            None => return Ok(None),
        };

    let executor = Arc::new(mcp_executor);

    // Process any approval responses
    let approval_messages =
        process_approval_responses(&executor, request, response_items_repository).await?;

    Ok(Some(McpSetupResult {
        executor,
        tool_definitions,
        approval_messages,
    }))
}

/// Extract cached mcp_list_tools from the request input.
///
/// Returns a map of server_label -> discovered tools that can be used
/// to skip tool discovery for servers that have cached tools.
pub fn extract_cached_mcp_tools(
    request: &models::CreateResponseRequest,
) -> HashMap<String, Vec<McpDiscoveredTool>> {
    let mut cached: HashMap<String, Vec<McpDiscoveredTool>> = HashMap::new();

    let items = match &request.input {
        Some(models::ResponseInput::Items(items)) => items,
        Some(models::ResponseInput::Text(_)) => return cached,
        None => return cached,
    };

    for item in items {
        if let models::ResponseInputItem::McpListTools {
            server_label,
            tools,
            ..
        } = item
        {
            cached.insert(server_label.clone(), tools.clone());
        }
    }

    cached
}

/// Initialize MCP connections for the given request tools.
///
/// Connects to remote MCP servers, discovers available tools, emits mcp_list_tools
/// items (for client-side caching), and returns the MCP executor and tool definitions.
pub async fn initialize_mcp_executor(
    request_tools: &[ResponseTool],
    cached_tools: &HashMap<String, Vec<McpDiscoveredTool>>,
    client_factory: Option<&Arc<dyn McpClientFactory>>,
    ctx: &mut ResponseStreamContext,
    emitter: &mut EventEmitter,
) -> Result<Option<(McpToolExecutor, Vec<ToolDefinition>)>, ResponseError> {
    let mcp_tools: Vec<&ResponseTool> = request_tools
        .iter()
        .filter(|t| matches!(t, ResponseTool::Mcp { .. }))
        .collect();

    if mcp_tools.is_empty() {
        return Ok(None);
    }

    // Use injected factory if provided (for testing), otherwise use default
    let mut mcp_executor = match client_factory {
        Some(factory) => McpToolExecutor::with_arc_client_factory(factory.clone()),
        None => McpToolExecutor::new(),
    };

    // Connect to servers, using cached tools where available
    let mcp_list_tools_items = mcp_executor
        .connect_servers(mcp_tools, cached_tools)
        .await?;

    for item in mcp_list_tools_items {
        let item_id = item.id().to_string();
        emitter.emit_item_added(ctx, item, item_id).await?;
    }

    // Get MCP tool definitions for the LLM
    let mcp_tool_defs = mcp_executor.get_tool_definitions();

    Ok(Some((mcp_executor, mcp_tool_defs)))
}

/// Process MCP approval responses from the request input.
///
/// For each approval response:
/// - Validates the approval request exists in the previous response
/// - If approved: executes the tool and adds result to messages
/// - If rejected: adds rejection message for LLM context
///
/// Returns the messages to add to the conversation context.
pub async fn process_approval_responses(
    mcp_executor: &McpToolExecutor,
    request: &models::CreateResponseRequest,
    response_items_repository: &Arc<dyn ResponseItemRepositoryTrait>,
) -> Result<Vec<crate::completions::ports::CompletionMessage>, ResponseError> {
    let mut messages = Vec::new();

    // Extract approval responses from input
    let approval_responses: Vec<_> = match &request.input {
        Some(models::ResponseInput::Items(items)) => items
            .iter()
            .filter_map(|item| item.as_mcp_approval())
            .collect(),
        _ => return Ok(messages),
    };

    if approval_responses.is_empty() {
        return Ok(messages);
    }

    // Load previous response items to validate approval requests
    let previous_response_id = request.previous_response_id.as_ref().ok_or_else(|| {
        ResponseError::InvalidParams(
            "MCP approval response requires previous_response_id".to_string(),
        )
    })?;

    // Parse the previous response ID to get the UUID
    let prev_response_uuid = previous_response_id
        .strip_prefix(crate::id_prefixes::PREFIX_RESP)
        .unwrap_or(previous_response_id);
    let prev_response_id =
        models::ResponseId(uuid::Uuid::parse_str(prev_response_uuid).map_err(|e| {
            ResponseError::InvalidParams(format!("Invalid previous_response_id: {e}"))
        })?);

    let previous_items = response_items_repository
        .list_by_response(prev_response_id)
        .await
        .map_err(|e| {
            ResponseError::InternalError(format!("Failed to load previous response items: {e}"))
        })?;

    // Process each approval response
    for (approval_request_id, approve) in approval_responses {
        if let Some(result_message) = mcp_executor
            .process_approval_response(approval_request_id, approve, &previous_items)
            .await?
        {
            messages.push(crate::completions::ports::CompletionMessage {
                role: "tool".to_string(),
                content: result_message,
            });
        }
    }

    Ok(messages)
}

// ============================================
// Tests
// ============================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses::models::McpApprovalMode;

    #[test]
    fn test_validate_server_url_https_required() {
        // HTTPS should pass
        assert!(McpToolExecutor::validate_server_url("https://example.com/mcp").is_ok());

        // HTTP should fail
        let result = McpToolExecutor::validate_server_url("http://example.com/mcp");
        assert!(matches!(result, Err(ResponseError::McpInsecureUrl)));
    }

    #[test]
    fn test_validate_server_url_blocks_private_ips() {
        // Localhost variants
        assert!(matches!(
            McpToolExecutor::validate_server_url("https://localhost/mcp"),
            Err(ResponseError::McpPrivateIpBlocked)
        ));
        assert!(matches!(
            McpToolExecutor::validate_server_url("https://127.0.0.1/mcp"),
            Err(ResponseError::McpPrivateIpBlocked)
        ));

        // Private IP ranges
        assert!(matches!(
            McpToolExecutor::validate_server_url("https://10.0.0.1/mcp"),
            Err(ResponseError::McpPrivateIpBlocked)
        ));
        assert!(matches!(
            McpToolExecutor::validate_server_url("https://172.16.0.1/mcp"),
            Err(ResponseError::McpPrivateIpBlocked)
        ));
        assert!(matches!(
            McpToolExecutor::validate_server_url("https://192.168.1.1/mcp"),
            Err(ResponseError::McpPrivateIpBlocked)
        ));

        // Public IPs should pass
        assert!(McpToolExecutor::validate_server_url("https://8.8.8.8/mcp").is_ok());
    }

    #[test]
    fn test_parse_tool_name() {
        assert_eq!(
            McpToolExecutor::parse_tool_name("myserver:mytool"),
            Some(("myserver", "mytool"))
        );
        assert_eq!(
            McpToolExecutor::parse_tool_name("server:tool:with:colons"),
            Some(("server", "tool:with:colons"))
        );
        assert_eq!(McpToolExecutor::parse_tool_name("notool"), None);
    }

    #[test]
    fn test_is_mcp_tool() {
        let mut executor = McpToolExecutor::new();
        executor
            .tool_to_server
            .insert("myserver:mytool".to_string(), "myserver".to_string());

        assert!(executor.is_mcp_tool("myserver:mytool"));
        assert!(!executor.is_mcp_tool("otherserver:othertool"));
        assert!(!executor.is_mcp_tool("regular_tool"));
    }

    #[tokio::test]
    async fn test_connect_servers_with_mock() {
        // Create mock client
        let mut mock_client = MockMcpClient::new();
        mock_client.expect_list_tools().returning(|| {
            Ok(vec![McpDiscoveredTool {
                name: "test_tool".to_string(),
                description: Some("A test tool".to_string()),
                input_schema: Some(serde_json::json!({"type": "object"})),
                annotations: None,
            }])
        });

        // Create mock factory
        let mut mock_factory = MockMcpClientFactory::new();
        mock_factory.expect_create_client().returning(move |_, _| {
            let mut client = MockMcpClient::new();
            client.expect_list_tools().returning(|| {
                Ok(vec![McpDiscoveredTool {
                    name: "test_tool".to_string(),
                    description: Some("A test tool".to_string()),
                    input_schema: Some(serde_json::json!({"type": "object"})),
                    annotations: None,
                }])
            });
            Ok(Box::new(client) as Box<dyn McpClient>)
        });

        let mut executor = McpToolExecutor::with_client_factory(Box::new(mock_factory));

        let mcp_tool = ResponseTool::Mcp {
            server_label: "test_server".to_string(),
            server_url: "https://example.com/mcp".to_string(),
            server_description: None,
            authorization: None,
            require_approval: McpApprovalRequirement::Simple(McpApprovalMode::Never),
            allowed_tools: None,
        };

        let result = executor
            .connect_servers(vec![&mcp_tool], &std::collections::HashMap::new())
            .await;
        assert!(result.is_ok());

        // Verify tool registration
        assert!(executor.is_mcp_tool("test_server:test_tool"));

        // Verify tool definitions are available
        let tool_defs = executor.get_tool_definitions();
        assert_eq!(tool_defs.len(), 1);
        assert_eq!(tool_defs[0].function.name, "test_server:test_tool");
    }

    #[tokio::test]
    async fn test_execute_tool_with_mock() {
        // Create mock factory that returns a client capable of executing tools
        let mut mock_factory = MockMcpClientFactory::new();
        mock_factory.expect_create_client().returning(|_, _| {
            let mut client = MockMcpClient::new();
            client.expect_list_tools().returning(|| {
                Ok(vec![McpDiscoveredTool {
                    name: "greet".to_string(),
                    description: Some("Greets someone".to_string()),
                    input_schema: None,
                    annotations: None,
                }])
            });
            client
                .expect_call_tool()
                .withf(|name, _| name == "greet")
                .returning(|_, _| Ok("Hello, World!".to_string()));
            Ok(Box::new(client) as Box<dyn McpClient>)
        });

        let mut executor = McpToolExecutor::with_client_factory(Box::new(mock_factory));

        let mcp_tool = ResponseTool::Mcp {
            server_label: "greeter".to_string(),
            server_url: "https://example.com/mcp".to_string(),
            server_description: None,
            authorization: None,
            require_approval: McpApprovalRequirement::Simple(McpApprovalMode::Never),
            allowed_tools: None,
        };

        // Connect first
        executor
            .connect_servers(vec![&mcp_tool], &std::collections::HashMap::new())
            .await
            .unwrap();

        // Execute tool
        let result = executor
            .execute_tool("greeter", "greet", serde_json::json!({"name": "World"}))
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Hello, World!");
    }

    #[test]
    fn test_requires_approval_simple_always() {
        let mut executor = McpToolExecutor::new();

        // Manually set up a connection with require_approval: always
        executor.connections.insert(
            "server1".to_string(),
            McpServerConnection {
                client: Box::new(MockMcpClient::new()),
                server_label: "server1".to_string(),
                tools: vec![],
                require_approval: McpApprovalRequirement::Simple(McpApprovalMode::Always),
            },
        );

        assert!(executor.requires_approval("server1", "any_tool"));
    }

    #[test]
    fn test_requires_approval_simple_never() {
        let mut executor = McpToolExecutor::new();

        executor.connections.insert(
            "server1".to_string(),
            McpServerConnection {
                client: Box::new(MockMcpClient::new()),
                server_label: "server1".to_string(),
                tools: vec![],
                require_approval: McpApprovalRequirement::Simple(McpApprovalMode::Never),
            },
        );

        assert!(!executor.requires_approval("server1", "any_tool"));
    }

    #[test]
    fn test_requires_approval_granular() {
        use crate::responses::models::McpToolNameFilter;
        use std::collections::HashSet;

        let mut executor = McpToolExecutor::new();

        let mut allowed_tools = HashSet::new();
        allowed_tools.insert("safe_tool".to_string());
        allowed_tools.insert("another_safe_tool".to_string());

        executor.connections.insert(
            "server1".to_string(),
            McpServerConnection {
                client: Box::new(MockMcpClient::new()),
                server_label: "server1".to_string(),
                tools: vec![],
                require_approval: McpApprovalRequirement::Granular {
                    never: McpToolNameFilter {
                        tool_names: allowed_tools,
                    },
                },
            },
        );

        // Tools in the "never" list don't require approval
        assert!(!executor.requires_approval("server1", "safe_tool"));
        assert!(!executor.requires_approval("server1", "another_safe_tool"));

        // Tools not in the list require approval
        assert!(executor.requires_approval("server1", "dangerous_tool"));
    }
}
