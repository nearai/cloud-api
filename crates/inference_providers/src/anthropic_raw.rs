//! Raw Anthropic Messages transport types.
//!
//! These types deliberately model HTTP transport, not the Messages schema. The
//! caller owns request validation and usage accounting; the provider only
//! rewrites the model, injects the upstream credential, and preserves the
//! upstream status, headers, and response bytes.

use bytes::Bytes;
use futures_core::Stream;
use http::{HeaderMap, StatusCode};
use std::pin::Pin;

/// Anthropic HTTP operation to invoke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicRawEndpoint {
    Messages,
    CountTokens,
}

impl AnthropicRawEndpoint {
    pub fn path(self) -> &'static str {
        match self {
            Self::Messages => "messages",
            Self::CountTokens => "messages/count_tokens",
        }
    }
}

/// Caller-controlled Anthropic headers that are safe to forward upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnthropicRawHeaders {
    pub version: Option<String>,
    pub beta: Option<String>,
}

/// A native Messages request after Cloud API authentication and policy checks.
#[derive(Debug, Clone)]
pub struct AnthropicRawRequest {
    pub endpoint: AnthropicRawEndpoint,
    /// Whether to select Anthropic's beta Messages surface (`?beta=true`).
    /// A boolean keeps arbitrary query strings out of the shared transport.
    pub beta: bool,
    pub body: serde_json::Value,
    pub headers: AnthropicRawHeaders,
}

/// Streaming response body returned by Anthropic.
pub type AnthropicRawBody =
    Pin<Box<dyn Stream<Item = Result<Bytes, AnthropicRawError>> + Send + 'static>>;

/// Verbatim upstream response metadata and byte stream.
pub struct AnthropicRawResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: AnthropicRawBody,
}

#[derive(Debug, thiserror::Error)]
pub enum AnthropicRawError {
    #[error("native Anthropic Messages transport is not supported by this provider")]
    UnsupportedProvider,
    #[error("invalid native Anthropic request: {0}")]
    InvalidRequest(String),
    #[error("Anthropic transport failed: {0}")]
    Transport(String),
}
