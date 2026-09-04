use axum::{
    body::Body, extract::Request, http::StatusCode, middleware::Next, response::Response,
    RequestExt,
};
use bytes::Bytes;
use http_body_util::{BodyExt, LengthLimitError};
use sha2::{Digest, Sha256};
use std::error::Error as _;
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

/// Middleware that hashes the request body and passes it to the next handler
///
/// This middleware reads the entire request body, computes its SHA-256 hash,
/// and makes both the hash and original body available to downstream handlers
/// via request extensions.
pub async fn body_hash_middleware(request: Request, next: Next) -> Result<Response, StatusCode> {
    let (parts, body) = request.with_limited_body().into_parts();

    let body_bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            let status = if error
                .source()
                .is_some_and(|source| source.is::<LengthLimitError>())
            {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            };
            tracing::warn!(%error, "Failed to read request body");
            return Err(status);
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

    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        extract::DefaultBodyLimit,
        http::{Request, StatusCode},
        middleware,
        response::IntoResponse,
        routing::post,
        Router,
    };
    use futures_util::stream;
    use std::{
        convert::Infallible,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
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
        let app = Router::new()
            .route("/test", post(test_handler))
            .layer(middleware::from_fn(body_hash_middleware));

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
        let app = Router::new()
            .route("/test", post(test_handler))
            .layer(middleware::from_fn(body_hash_middleware));

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

    #[tokio::test]
    async fn test_body_hash_rejects_chunked_body_over_configured_limit() {
        // Given: a streaming body without Content-Length and a four-byte route limit.
        let handler_called = Arc::new(AtomicBool::new(false));
        let app = Router::new()
            .route(
                "/test",
                post({
                    let handler_called = Arc::clone(&handler_called);
                    move || {
                        let handler_called = Arc::clone(&handler_called);
                        async move {
                            handler_called.store(true, Ordering::SeqCst);
                            StatusCode::OK
                        }
                    }
                }),
            )
            .layer(middleware::from_fn(body_hash_middleware))
            .layer(DefaultBodyLimit::max(4));
        let body = Body::from_stream(stream::iter([
            Ok::<_, Infallible>(Bytes::from_static(b"123")),
            Ok::<_, Infallible>(Bytes::from_static(b"45")),
        ]));
        let request = Request::builder()
            .method("POST")
            .uri("/test")
            .body(body)
            .unwrap();

        // When: the body-hash middleware consumes the body.
        let response = app.oneshot(request).await.unwrap();

        // Then: it rejects at the configured boundary before the handler runs.
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(!handler_called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_body_hash_allows_body_at_configured_limit() {
        // Given: a streaming body exactly equal to the configured route limit.
        let app = Router::new()
            .route("/test", post(test_handler))
            .layer(middleware::from_fn(body_hash_middleware))
            .layer(DefaultBodyLimit::max(4));
        let body = Body::from_stream(stream::iter([
            Ok::<_, Infallible>(Bytes::from_static(b"12")),
            Ok::<_, Infallible>(Bytes::from_static(b"34")),
        ]));
        let request = Request::builder()
            .method("POST")
            .uri("/test")
            .body(body)
            .unwrap();

        // When: the body-hash middleware consumes the body.
        let response = app.oneshot(request).await.unwrap();

        // Then: the request reaches the handler and hashes all four bytes.
        assert_eq!(response.status(), StatusCode::OK);
        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let expected_hash = hex::encode(Sha256::digest(b"1234"));
        assert_eq!(body_bytes, expected_hash.as_bytes());
    }
}
