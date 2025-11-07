#[derive(Debug, thiserror::Error)]
pub enum ResponseError {
    #[error("Invalid parameters: {0}")]
    InvalidParams(String),
    #[error("Internal error: {0}")]
    InternalError(String),
    #[error("Unknown tool: {0}. Available tools are: web_search, file_search, current_date. Please use one of these valid tool names")]
    UnknownTool(String),
    #[error("Tool call is missing a tool name. Please ensure all tool calls include a valid 'name' field. Available tools: web_search, file_search, current_date")]
    EmptyToolName,
}
