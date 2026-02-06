#[derive(Debug, thiserror::Error)]
pub enum ResponseError {
    #[error("Invalid parameters: {0}")]
    InvalidParams(String),
    #[error("Internal error: {0}")]
    InternalError(String),
    #[error("Unknown tool: {0}. Available tools are: web_search, file_search. Please use one of these valid tool names")]
    UnknownTool(String),
    #[error("Tool call is missing a tool name. Please ensure all tool calls include a valid 'name' field. Available tools: web_search, file_search")]
    EmptyToolName,
    #[error("Stream interrupted")]
    StreamInterrupted,

    // ============================================
    // MCP (Model Context Protocol) Errors
    // ============================================
    #[error("MCP connection failed: {0}")]
    McpConnectionFailed(String),

    #[error("MCP tool discovery failed: {0}")]
    McpToolDiscoveryFailed(String),

    #[error("MCP tool execution failed: {0}")]
    McpToolExecutionFailed(String),

    #[error("MCP server limit exceeded: max {max} servers per request")]
    McpServerLimitExceeded { max: usize },

    #[error("MCP tool limit exceeded: server '{server}' has {count} tools, max {max}")]
    McpToolLimitExceeded {
        server: String,
        count: usize,
        max: usize,
    },

    #[error("MCP server URL must use HTTPS")]
    McpInsecureUrl,

    #[error("MCP private IP addresses not allowed")]
    McpPrivateIpBlocked,

    #[error("MCP approval required for tool '{tool}' on server '{server}'")]
    McpApprovalRequired { server: String, tool: String },

    #[error("MCP approval request not found: {0}")]
    McpApprovalRequestNotFound(String),

    // ============================================
    // Function Tool Errors
    // ============================================
    #[error("Function call required: {name} (call_id: {call_id})")]
    FunctionCallRequired { name: String, call_id: String },

    #[error("Function call not found: {0}")]
    FunctionCallNotFound(String),
}
