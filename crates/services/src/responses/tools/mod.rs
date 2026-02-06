pub mod brave;
pub mod executor;
pub mod file_search;
pub mod function;
pub mod mcp;
pub mod ports;
pub mod tool_config;
pub mod web_search;

// Executor framework
pub use executor::{
    FunctionCallInfo, ToolEventContext, ToolExecutionContext, ToolExecutionResult, ToolExecutor,
    ToolOutput, ToolRegistry, MAX_CONSECUTIVE_TOOL_FAILURES,
};

// Function tools
pub use function::FunctionToolExecutor;

// Tool configuration helpers
pub use tool_config::{
    convert_tool_calls, get_function_tool_names, get_tool_names, prepare_tool_choice,
    prepare_tools, CODE_INTERPRETER_TOOL_NAME, COMPUTER_TOOL_NAME, ERROR_TOOL_TYPE,
};

// Tool executors
pub use file_search::{FileSearchToolExecutor, FILE_SEARCH_TOOL_NAME};
pub use web_search::{
    FormattedWebSearchResult, WebSearchToolExecutor, CITATION_INSTRUCTION, WEB_SEARCH_TOOL_NAME,
};

// MCP
pub use mcp::{
    setup_mcp, McpClientFactory, McpSetupResult, McpToolExecutor, MAX_MCP_SERVERS_PER_REQUEST,
    MAX_TOOLS_PER_SERVER,
};

#[cfg(any(test, feature = "test-mocks"))]
pub use mcp::{MockMcpClient, MockMcpClientFactory};

// Provider traits and types
pub use ports::{
    FileSearchProviderTrait, FileSearchResult, WebSearchError, WebSearchParams,
    WebSearchProviderTrait, WebSearchResult,
};
