pub mod brave;
pub mod executor;
pub mod file_search;
pub mod mcp;
pub mod ports;
pub mod web_search;

// Executor framework
pub use executor::{
    ToolEventContext, ToolExecutionContext, ToolExecutor, ToolOutput, ToolRegistry,
};

// Tool executors
pub use file_search::{FileSearchToolExecutor, FILE_SEARCH_TOOL_NAME};
pub use web_search::{FormattedWebSearchResult, WebSearchToolExecutor, WEB_SEARCH_TOOL_NAME};

// MCP
pub use mcp::{
    McpClientFactory, McpToolExecutor, MAX_MCP_SERVERS_PER_REQUEST, MAX_TOOLS_PER_SERVER,
};

#[cfg(any(test, feature = "test-mocks"))]
pub use mcp::{MockMcpClient, MockMcpClientFactory};

// Provider traits and types
pub use ports::{
    FileSearchProviderTrait, FileSearchResult, WebSearchError, WebSearchParams,
    WebSearchProviderTrait, WebSearchResult,
};
