//! Native Responses transport. No Chat Completions conversion or server state.
use bytes::Bytes;
use futures_core::Stream;
use std::pin::Pin;

pub type ResponsesRawBody =
    Pin<Box<dyn Stream<Item = Result<Bytes, crate::CompletionError>> + Send>>;

pub struct ResponsesRawResponse {
    pub status: http::StatusCode,
    pub headers: http::HeaderMap,
    pub body: ResponsesRawBody,
}

pub fn is_astra(model: &str) -> bool {
    model == "gpt-6-astra" || model.starts_with("gpt-6-astra-")
}

/// Explicitly opt into a request with no gateway or upstream retained state.
/// Null optional fields have the same meaning as omitted fields.
pub fn is_stateless(body: &serde_json::Value) -> bool {
    body.get("store") == Some(&serde_json::Value::Bool(false))
        && body
            .get("conversation")
            .is_none_or(serde_json::Value::is_null)
        && body
            .get("previous_response_id")
            .is_none_or(serde_json::Value::is_null)
        && body
            .get("background")
            .is_none_or(|v| v.is_null() || v == false)
}
