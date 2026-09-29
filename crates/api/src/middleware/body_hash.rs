use crate::models::ErrorResponse;
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header::CONTENT_LENGTH, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use http_body_util::{BodyExt, LengthLimitError, Limited};
use sha2::{Digest, Sha256};
use tracing::debug;

/// Hashed request body information passed to route handlers
#[derive(Clone, Debug)]
pub struct RequestBodyHash {
    /// SHA-256 hash of the request body as a hex string
    pub hash: String,
    /// Original body bytes (for reference if needed)
    pub body_bytes: Bytes,
}

impl RequestBodyHash {
    /// Get the hash as a hex string
    pub fn as_hex(&self) -> &str {
        &self.hash
    }

    /// Get the hash as bytes
    pub fn as_bytes(&self) -> Vec<u8> {
        hex::decode(&self.hash).unwrap_or_default()
    }
}

/// Maximum number of body bytes `body_hash_middleware` will buffer.
///
/// The middleware runs before API-key auth, and per-route `DefaultBodyLimit`
/// is only enforced by extractors after it has already buffered the body, so
/// each route group must pass the largest limit of the routes it wraps.
#[derive(Clone, Copy, Debug)]
pub struct BodyHashLimit(pub usize);

fn payload_too_large(limit: usize) -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        Json(ErrorResponse::new(
            format!("Request body exceeds the maximum allowed size of {limit} bytes"),
            "invalid_request_error".to_string(),
        )),
    )
        .into_response()
}

/// Middleware that hashes the request body and passes it to the next handler
///
/// This middleware reads the request body (up to `limit` bytes), computes its
/// SHA-256 hash, and makes both the hash and original body available to
/// downstream handlers via request extensions. Larger bodies are rejected with
/// 413 without being read past the limit.
pub async fn body_hash_middleware(
    State(BodyHashLimit(limit)): State<BodyHashLimit>,
    request: Request,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();

    let declared_len = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared_len.is_some_and(|len| len > limit as u64) {
        return payload_too_large(limit);
    }

    // Collect the body, stopping as soon as it exceeds the limit
    let body_bytes = match Limited::new(body, limit).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) if e.is::<LengthLimitError>() => return payload_too_large(limit),
        Err(_) => {
            tracing::warn!("Failed to read request body");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    // Compute SHA-256 hash of the body
    let mut hasher = Sha256::new();
    hasher.update(&body_bytes);
    let hash_bytes = hasher.finalize();
    let hash = hex::encode(hash_bytes);

    debug!(
        "Request body hash computed: {} (body size: {} bytes)",
        hash,
        body_bytes.len()
    );

    // Create the hash info struct
    let body_hash = RequestBodyHash {
        hash,
        body_bytes: body_bytes.clone(),
    };

    // Reconstruct the request with the original body
    let mut request = Request::from_parts(parts, Body::from(body_bytes));

    // Add the hash to request extensions for downstream handlers
    request.extensions_mut().insert(body_hash);

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
        middleware,
        response::IntoResponse,
        routing::post,
        Router,
    };
    use tower::ServiceExt;

    async fn test_handler(request: Request<Body>) -> impl IntoResponse {
        let body_hash = request
            .extensions()
            .get::<RequestBodyHash>()
            .expect("RequestBodyHash should be present");

        (StatusCode::OK, body_hash.hash.clone())
    }

    #[tokio::test]
    async fn test_body_hash_middleware() {
        let app =
            Router::new()
                .route("/test", post(test_handler))
                .layer(middleware::from_fn_with_state(
                    BodyHashLimit(1024),
                    body_hash_middleware,
                ));

        let request = Request::builder()
            .method("POST")
            .uri("/test")
            .body(Body::from("test body content"))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // Verify the hash is correct for "test body content"
        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let hash = String::from_utf8(body_bytes.to_vec()).unwrap();

        // Expected SHA-256 hash of "test body content"
        let mut hasher = Sha256::new();
        hasher.update(b"test body content");
        let expected_hash = hex::encode(hasher.finalize());

        assert_eq!(hash, expected_hash);
    }

    #[tokio::test]
    async fn test_empty_body_hash() {
        let app =
            Router::new()
                .route("/test", post(test_handler))
                .layer(middleware::from_fn_with_state(
                    BodyHashLimit(1024),
                    body_hash_middleware,
                ));

        let request = Request::builder()
            .method("POST")
            .uri("/test")
            .body(Body::from(""))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // Verify the hash is correct for empty body
        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let hash = String::from_utf8(body_bytes.to_vec()).unwrap();

        // Expected SHA-256 hash of empty string
        let mut hasher = Sha256::new();
        hasher.update(b"");
        let expected_hash = hex::encode(hasher.finalize());

        assert_eq!(hash, expected_hash);
    }

    fn limited_app(limit: usize) -> Router {
        Router::new()
            .route("/test", post(test_handler))
            .layer(middleware::from_fn_with_state(
                BodyHashLimit(limit),
                body_hash_middleware,
            ))
    }

    #[tokio::test]
    async fn test_body_at_limit_is_accepted() {
        let request = Request::builder()
            .method("POST")
            .uri("/test")
            .body(Body::from(vec![b'a'; 16]))
            .unwrap();

        let response = limited_app(16).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_chunked_body_over_limit_stops_reading() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let consumed = Arc::new(AtomicUsize::new(0));
        let counter = consumed.clone();
        let stream = futures::stream::iter((0..100).map(move |_| {
            counter.fetch_add(8, Ordering::SeqCst);
            Ok::<_, std::io::Error>(Bytes::from_static(b"aaaaaaaa"))
        }));
        let request = Request::builder()
            .method("POST")
            .uri("/test")
            .body(Body::from_stream(stream))
            .unwrap();

        let response = limited_app(64).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(consumed.load(Ordering::SeqCst) <= 64 + 8);
        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(json["error"]["type"], "invalid_request_error");
    }

    #[tokio::test]
    async fn test_declared_content_length_over_limit_is_rejected() {
        let request = Request::builder()
            .method("POST")
            .uri("/test")
            .header("Content-Length", "65")
            .body(Body::empty())
            .unwrap();

        let response = limited_app(64).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
