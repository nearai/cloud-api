pub mod attestation;
pub mod auth;
pub mod completions;
pub mod conversations;
pub mod inference_provider_pool;
pub mod mcp;
pub mod models;
pub mod organization;
pub mod responses;

// Re-export commonly used types for backward compatibility
pub use completions::{
    ports::{CompletionError, CompletionMessage, CompletionRequest},
    CompletionServiceImpl,
};

// Re-export inference provider types that are commonly used
pub use inference_providers::{
    ChatCompletionParams, ChatMessage, CompletionParams, FinishReason, MessageRole, TokenUsage,
};

pub use responses::{
    ports::{
        ConversationId, Response, ResponseError, ResponseId, ResponseInput, ResponseMessage,
        ResponseRequest, ResponseStatus, ResponseStreamEvent,
    },
    ResponseService,
};

pub use auth::UserId;

pub use conversations::{
    ports::{Conversation, ConversationError},
    ConversationService,
};

pub use mcp::{ports::McpError, McpClientManager};
