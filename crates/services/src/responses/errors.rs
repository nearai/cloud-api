#[derive(Debug, thiserror::Error)]
pub enum ResponseError {
    #[error("Invalid parameters: {0}")]
    InvalidParams(String),
    #[error("Internal error: {0}")]
    InternalError(String),
    #[error(transparent)]
    Completion(#[from] crate::completions::CompletionError),
    #[error("Unknown tool: {0}. Available tools are: web_search, web_context_search, file_search. Please use one of these valid tool names")]
    UnknownTool(String),
    #[error("Tool call is missing a tool name. Please ensure all tool calls include a valid 'name' field. Available tools: web_search, web_context_search, file_search")]
    EmptyToolName,
    #[error("Stream interrupted")]
    StreamInterrupted,

    /// The referenced conversation does not exist in the caller's workspace.
    /// Unknown and foreign conversation IDs are deliberately indistinguishable
    /// (non-enumerating 404).
    #[error("Conversation not found")]
    ConversationNotFound,

    /// The referenced previous response does not exist in the caller's
    /// workspace. Unknown and foreign response IDs are deliberately
    /// indistinguishable (non-enumerating 404).
    #[error("Previous response not found")]
    PreviousResponseNotFound,

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

impl ResponseError {
    pub fn http_status_code(&self) -> u16 {
        match self {
            ResponseError::InvalidParams(_)
            | ResponseError::UnknownTool(_)
            | ResponseError::EmptyToolName
            | ResponseError::McpServerLimitExceeded { .. }
            | ResponseError::McpToolLimitExceeded { .. }
            | ResponseError::McpInsecureUrl
            | ResponseError::McpPrivateIpBlocked
            | ResponseError::McpApprovalRequired { .. }
            | ResponseError::FunctionCallRequired { .. } => 400,
            ResponseError::McpApprovalRequestNotFound(_)
            | ResponseError::FunctionCallNotFound(_)
            | ResponseError::ConversationNotFound
            | ResponseError::PreviousResponseNotFound => 404,
            ResponseError::McpConnectionFailed(_)
            | ResponseError::McpToolDiscoveryFailed(_)
            | ResponseError::McpToolExecutionFailed(_) => 502,
            ResponseError::Completion(error) => completion_http_status_code(error),
            ResponseError::InternalError(_) | ResponseError::StreamInterrupted => 500,
        }
    }

    /// Whether this failure was caused by the client's request (bad params,
    /// unknown tool, the org's own rate limit, provider-rejected params mapped
    /// to `InvalidParams` upstream) rather than by our infrastructure.
    ///
    /// Deliberately variant-based, not "status is 4xx":
    /// `CompletionError::ProviderError` passes the raw upstream status
    /// through, where 401/403/407 mean OUR backend credentials are broken and
    /// 408 is an upstream timeout — those must keep logging at ERROR.
    pub fn is_client_caused(&self) -> bool {
        match self {
            ResponseError::InvalidParams(_)
            | ResponseError::UnknownTool(_)
            | ResponseError::EmptyToolName
            | ResponseError::McpServerLimitExceeded { .. }
            | ResponseError::McpToolLimitExceeded { .. }
            | ResponseError::McpInsecureUrl
            | ResponseError::McpPrivateIpBlocked
            | ResponseError::McpApprovalRequired { .. }
            | ResponseError::McpApprovalRequestNotFound(_)
            | ResponseError::FunctionCallRequired { .. }
            | ResponseError::FunctionCallNotFound(_)
            | ResponseError::ConversationNotFound
            | ResponseError::PreviousResponseNotFound => true,
            ResponseError::Completion(error) => matches!(
                error,
                crate::completions::CompletionError::InvalidModel(_)
                    | crate::completions::CompletionError::InvalidParams(_)
                    | crate::completions::CompletionError::RateLimitExceeded(_)
            ),
            ResponseError::InternalError(_)
            | ResponseError::StreamInterrupted
            | ResponseError::McpConnectionFailed(_)
            | ResponseError::McpToolDiscoveryFailed(_)
            | ResponseError::McpToolExecutionFailed(_) => false,
        }
    }

    pub fn response_error(&self) -> crate::responses::models::ResponseError {
        match self {
            ResponseError::InvalidParams(msg) => response_error(msg, "invalid_request_error", None),
            ResponseError::InternalError(msg) => response_error(
                &format!("Internal server error: {msg}"),
                "internal_server_error",
                None,
            ),
            ResponseError::Completion(error) => completion_response_error(error),
            ResponseError::UnknownTool(msg) => response_error(
                &format!("Unknown tool: {msg}"),
                "invalid_request_error",
                None,
            ),
            ResponseError::EmptyToolName => response_error(
                "Tool call is missing a tool name",
                "invalid_request_error",
                None,
            ),
            ResponseError::StreamInterrupted => {
                response_error("Stream interrupted", "stream_error", None)
            }
            ResponseError::ConversationNotFound => {
                response_error("Conversation not found", "not_found", None)
            }
            ResponseError::PreviousResponseNotFound => {
                response_error("Previous response not found", "not_found", None)
            }
            ResponseError::McpConnectionFailed(msg) => {
                response_error(&format!("MCP connection failed: {msg}"), "mcp_error", None)
            }
            ResponseError::McpToolDiscoveryFailed(msg) => response_error(
                &format!("MCP tool discovery failed: {msg}"),
                "mcp_error",
                None,
            ),
            ResponseError::McpToolExecutionFailed(msg) => response_error(
                &format!("MCP tool execution failed: {msg}"),
                "mcp_error",
                None,
            ),
            ResponseError::McpServerLimitExceeded { max } => response_error(
                &format!("MCP server limit exceeded: max {max} servers per request"),
                "invalid_request_error",
                None,
            ),
            ResponseError::McpToolLimitExceeded { server, count, max } => response_error(
                &format!("MCP tool limit exceeded: server '{server}' has {count} tools, max {max}"),
                "invalid_request_error",
                None,
            ),
            ResponseError::McpInsecureUrl => response_error(
                "MCP server URL must use HTTPS",
                "invalid_request_error",
                None,
            ),
            ResponseError::McpPrivateIpBlocked => response_error(
                "MCP private IP addresses not allowed",
                "invalid_request_error",
                None,
            ),
            ResponseError::McpApprovalRequired { server, tool } => response_error(
                &format!("MCP approval required for tool '{tool}' on server '{server}'"),
                "mcp_approval_required",
                None,
            ),
            ResponseError::McpApprovalRequestNotFound(msg) => {
                response_error(msg, "not_found", Some("approval_request_not_found"))
            }
            ResponseError::FunctionCallRequired { name, call_id } => response_error(
                &format!("Function call required: {name} (call_id: {call_id})"),
                "function_call_required",
                None,
            ),
            ResponseError::FunctionCallNotFound(msg) => {
                response_error(msg, "not_found", Some("function_call_not_found"))
            }
        }
    }
}

fn completion_http_status_code(error: &crate::completions::CompletionError) -> u16 {
    match error {
        crate::completions::CompletionError::InvalidModel(_)
        | crate::completions::CompletionError::InvalidParams(_) => 400,
        crate::completions::CompletionError::RateLimitExceeded(_) => 429,
        crate::completions::CompletionError::ProviderError { status_code, .. } => *status_code,
        crate::completions::CompletionError::ServiceOverloaded(_) => 429,
        crate::completions::CompletionError::InternalError(_) => 500,
    }
}

fn completion_response_error(
    error: &crate::completions::CompletionError,
) -> crate::responses::models::ResponseError {
    match error {
        crate::completions::CompletionError::InvalidModel(msg) => {
            let mut error = response_error(msg, "invalid_request_error", None);
            error.param = Some("model".to_string());
            error
        }
        crate::completions::CompletionError::InvalidParams(msg) => {
            response_error(msg, "invalid_request_error", None)
        }
        crate::completions::CompletionError::RateLimitExceeded(msg) => {
            let message = if msg.is_empty() {
                "Rate limit exceeded"
            } else {
                msg
            };
            response_error(message, "rate_limit_exceeded", None)
        }
        crate::completions::CompletionError::ProviderError {
            status_code,
            message,
        } => {
            let error_type = match status_code {
                502 => "bad_gateway",
                503 => "service_overloaded",
                504 => "gateway_timeout",
                _ => "provider_error",
            };
            response_error(message, error_type, None)
        }
        crate::completions::CompletionError::ServiceOverloaded(msg) => {
            response_error(msg, "service_overloaded", None)
        }
        crate::completions::CompletionError::InternalError(msg) => response_error(
            &format!("Internal server error: {msg}"),
            "internal_server_error",
            None,
        ),
    }
}

fn response_error(
    message: &str,
    error_type: &str,
    code: Option<&str>,
) -> crate::responses::models::ResponseError {
    crate::responses::models::ResponseError {
        message: message.to_string(),
        type_: error_type.to_string(),
        param: None,
        code: code.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completions::CompletionError;

    #[test]
    fn test_responses_api_service_overloaded_returns_429() {
        let completion_err = CompletionError::ServiceOverloaded("all backends busy".to_string());
        let response_err = ResponseError::Completion(completion_err);
        assert_eq!(
            response_err.http_status_code(),
            429,
            "ServiceOverloaded should map to HTTP 429 in the Responses API"
        );
    }

    #[test]
    fn test_responses_api_service_overloaded_error_type() {
        let completion_err = CompletionError::ServiceOverloaded("all backends busy".to_string());
        let response_err = ResponseError::Completion(completion_err);
        let error_body = response_err.response_error();
        assert_eq!(
            error_body.type_,
            "service_overloaded",
            "ServiceOverloaded should carry type=service_overloaded in the Responses API error body"
        );
    }

    #[test]
    fn test_responses_api_rate_limit_returns_429() {
        let completion_err = CompletionError::RateLimitExceeded("quota exceeded".to_string());
        let response_err = ResponseError::Completion(completion_err);
        assert_eq!(response_err.http_status_code(), 429);
        assert_eq!(response_err.response_error().type_, "rate_limit_exceeded");
    }
}
