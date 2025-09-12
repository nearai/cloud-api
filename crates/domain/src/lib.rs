// Domain Layer - Business Logic and Models
//
// This crate contains the core domain logic for AI completions, organized into:
// - models: Core domain models and data structures
// - services: Service traits and implementations
// - errors: Error types and handling
//
// The domain layer is technology-agnostic and focuses purely on business logic.

pub mod auth;
pub mod errors;
pub mod mcp;
pub mod models;
pub mod providers;
pub mod services;

// Re-export all public types for convenience
pub use errors::CompletionError;
pub use models::*;
pub use providers::{
    StreamChunk, StreamChoice, Delta, 
    ModelInfo, ModelsResponse, 
    vllm::VLlmProvider, 
    mock::MockProvider
};
pub use services::{
    CompletionHandler, Domain, ProviderRouter, MockCompletionHandler
};
pub use auth::*;
