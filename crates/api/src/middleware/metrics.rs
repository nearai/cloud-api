//! HTTP metrics middleware for tracking request counts and latencies.
//!
//! This middleware records low-cardinality metrics for all HTTP requests:
//! - `cloud_api.http.requests` - Count of HTTP requests by method, endpoint, status
//! - `cloud_api.http.duration` - Histogram of request durations by method, endpoint
//!
//! Endpoints are normalized to replace UUIDs with `{id}` to reduce cardinality.

use axum::{body::Body, extract::State, http::Request, middleware::Next, response::Response};
use services::metrics::{
    consts::{
        get_environment, METRIC_HTTP_DURATION, METRIC_HTTP_REQUESTS, TAG_ENDPOINT, TAG_ENVIRONMENT,
        TAG_METHOD, TAG_STATUS_CODE,
    },
    MetricsServiceTrait,
};
use std::sync::Arc;
use std::time::Instant;

/// State for the metrics middleware
#[derive(Clone)]
pub struct MetricsState {
    pub metrics_service: Arc<dyn MetricsServiceTrait>,
}

/// Middleware that records HTTP request metrics
pub async fn http_metrics_middleware(
    State(state): State<MetricsState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let start = Instant::now();
    let method = req.method().to_string();
    let path = req.uri().path().to_string();

    let response = next.run(req).await;
    let duration = start.elapsed();
    let status = response.status().as_u16();

    // Normalize path to reduce cardinality (replace UUIDs with {id})
    let endpoint = normalize_path(&path);
    let environment = get_environment();

    let tags = [format!("{TAG_METHOD}:{method}"),
        format!("{TAG_ENDPOINT}:{endpoint}"),
        format!("{TAG_STATUS_CODE}:{status}"),
        format!("{TAG_ENVIRONMENT}:{environment}")];
    let tags_str: Vec<&str> = tags.iter().map(|s| s.as_str()).collect();

    state
        .metrics_service
        .record_latency(METRIC_HTTP_DURATION, duration, &tags_str);
    state
        .metrics_service
        .record_count(METRIC_HTTP_REQUESTS, 1, &tags_str);

    response
}

/// Normalize path by replacing UUIDs and dynamic IDs with `{id}` to reduce cardinality.
///
/// Examples:
/// - `/v1/workspaces/abc12345-1234-5678-9abc-def012345678/api-keys` -> `/v1/workspaces/{id}/api-keys`
/// - `/v1/responses/resp_abc123` -> `/v1/responses/{id}`
fn normalize_path(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if is_uuid(segment) || is_dynamic_id(segment) {
                "{id}"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Check if a string looks like a UUID (8-4-4-4-12 hex pattern)
fn is_uuid(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    let expected_lens = [8, 4, 4, 4, 12];
    parts
        .iter()
        .zip(expected_lens.iter())
        .all(|(part, &len)| part.len() == len && part.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Check if a string looks like a dynamic ID (e.g., chatcmpl-xxx, resp_xxx, sk-xxx)
fn is_dynamic_id(s: &str) -> bool {
    // Common prefixes for dynamic IDs
    let prefixes = [
        "chatcmpl-",
        "resp_",
        "file-",
        "msg_",
        "run_",
        "thread_",
        "asst_",
        "conv_",
        "sk-",
    ];
    prefixes.iter().any(|prefix| s.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_uuid() {
        assert!(is_uuid("abc12345-1234-5678-9abc-def012345678"));
        assert!(is_uuid("ABC12345-1234-5678-9ABC-DEF012345678"));
        assert!(!is_uuid("not-a-uuid"));
        assert!(!is_uuid("abc12345-1234-5678-9abc")); // too short
        assert!(!is_uuid("abc12345-1234-5678-9abc-def012345678x")); // too long
    }

    #[test]
    fn test_is_dynamic_id() {
        assert!(is_dynamic_id("chatcmpl-abc123xyz"));
        assert!(is_dynamic_id("resp_abc123"));
        assert!(is_dynamic_id("sk-test123"));
        assert!(!is_dynamic_id("models"));
        assert!(!is_dynamic_id("health"));
    }

    #[test]
    fn test_normalize_path() {
        // UUID normalization
        assert_eq!(
            normalize_path("/v1/workspaces/abc12345-1234-5678-9abc-def012345678/api-keys"),
            "/v1/workspaces/{id}/api-keys"
        );

        // Multiple UUIDs
        assert_eq!(
            normalize_path("/v1/workspaces/abc12345-1234-5678-9abc-def012345678/api-keys/def12345-1234-5678-9abc-def012345678"),
            "/v1/workspaces/{id}/api-keys/{id}"
        );

        // OpenAI-style IDs
        assert_eq!(
            normalize_path("/v1/responses/chatcmpl-abc123xyz"),
            "/v1/responses/{id}"
        );
        assert_eq!(
            normalize_path("/v1/responses/resp_abc123"),
            "/v1/responses/{id}"
        );

        // No IDs to normalize
        assert_eq!(normalize_path("/v1/models"), "/v1/models");
        assert_eq!(normalize_path("/health"), "/health");
    }
}
