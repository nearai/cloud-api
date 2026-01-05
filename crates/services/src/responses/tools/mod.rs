pub mod brave;
pub mod mcp;
pub mod ports;

pub use mcp::{
    McpClientFactory, McpToolExecutor, MAX_MCP_SERVERS_PER_REQUEST, MAX_TOOLS_PER_SERVER,
};

#[cfg(any(test, feature = "test-mocks"))]
pub use mcp::{MockMcpClient, MockMcpClientFactory};
pub use ports::{
    FileSearchProviderTrait, WebSearchError, WebSearchParams, WebSearchProviderTrait,
    WebSearchResult,
};
