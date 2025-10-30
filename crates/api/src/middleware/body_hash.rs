use axum::{body::Body, extract::Request, http::StatusCode, middleware::Next, response::Response};
use bytes::Bytes;
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use tracing::{debug, error};

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
    let (parts, body) = request.into_parts();

    // Collect the entire body
    let body_bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => {
            error!("Failed to read request body");
            return Err(StatusCode::BAD_REQUEST);
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
}
