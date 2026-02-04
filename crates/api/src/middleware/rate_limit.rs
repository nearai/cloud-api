use axum::{
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::Response,
};
use moka::future::Cache;
use std::{
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};
use tracing::{debug, warn};

use super::auth::AuthenticatedApiKey;
use crate::models::ErrorResponse;

const DEFAULT_API_KEY_RATE_LIMIT: u32 = 1000; // requests per minute
const DEFAULT_IMAGE_RATE_LIMIT: u32 = 10; // image operations per minute (100x more expensive)
const RATE_LIMIT_WINDOW_SECS: u64 = 60;
const RATE_LIMIT_CACHE_MAX_CAPACITY: u64 = 50_000;

#[derive(Debug)]
struct Counter(AtomicU32);

impl Counter {
    fn new(value: u32) -> Self {
        Self(AtomicU32::new(value))
    }

    fn increment(&self) -> u32 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }
}

#[derive(Clone)]
pub struct RateLimitState {
    /// General request rate limiter (text operations, non-image endpoints)
    key_limits: Cache<String, Arc<Counter>>,
    /// Image-specific rate limiter (image generation, edits - 100x more expensive)
    image_limits: Cache<String, Arc<Counter>>,
    /// General rate limit (requests per minute)
    rate_limit: u32,
    /// Image rate limit (image operations per minute)
    image_rate_limit: u32,
}

impl Default for RateLimitState {
    fn default() -> Self {
        Self::new(DEFAULT_API_KEY_RATE_LIMIT, DEFAULT_IMAGE_RATE_LIMIT)
    }
}

impl RateLimitState {
    /// Create a new rate limiter with separate text and image limits
    pub fn new(rate_limit: u32, image_rate_limit: u32) -> Self {
        let window = Duration::from_secs(RATE_LIMIT_WINDOW_SECS);

        let key_limits: Cache<String, Arc<Counter>> = Cache::builder()
            .time_to_live(window)
            .max_capacity(RATE_LIMIT_CACHE_MAX_CAPACITY)
            .build();

        let image_limits: Cache<String, Arc<Counter>> = Cache::builder()
            .time_to_live(window)
            .max_capacity(RATE_LIMIT_CACHE_MAX_CAPACITY)
            .build();

        Self {
            key_limits,
            image_limits,
            rate_limit,
            image_rate_limit,
        }
    }

    /// Check general rate limit (for text operations)
    async fn check_limit(&self, api_key_id: &str) -> (bool, u32, u32) {
        let counter = self
            .key_limits
            .get_with(api_key_id.to_string(), async { Arc::new(Counter::new(0)) })
            .await;

        let count = counter.increment();
        let allowed = count <= self.rate_limit;

        (allowed, count, self.rate_limit)
    }

    /// Check image-specific rate limit
    /// Images are 100x more expensive than text operations
    async fn check_image_limit(&self, api_key_id: &str) -> (bool, u32, u32) {
        let counter = self
            .image_limits
            .get_with(api_key_id.to_string(), async { Arc::new(Counter::new(0)) })
            .await;

        let count = counter.increment();
        let allowed = count <= self.image_rate_limit;

        (allowed, count, self.image_rate_limit)
    }
}

/// Determine if a request is image-related based on path
fn is_image_operation(path: &str, method: &Method) -> bool {
    // Only POST requests to image endpoints are rate-limited as image operations
    if *method != Method::POST {
        return false;
    }

    // Image-specific endpoints
    path.contains("/images/generations") || path.contains("/images/edits")
}

pub async fn api_key_rate_limit_middleware(
    State(state): State<RateLimitState>,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, axum::Json<ErrorResponse>)> {
    let auth_key = match request.extensions().get::<AuthenticatedApiKey>() {
        Some(key) => key.clone(),
        None => return Ok(next.run(request).await),
    };

    let api_key_id = &auth_key.api_key.id.0;
    let is_image = is_image_operation(request.uri().path(), request.method());

    if is_image {
        // Image operations have separate, stricter rate limits
        let (allowed, count, limit) = state.check_image_limit(api_key_id).await;

        if !allowed {
            warn!(
                "Image rate limit exceeded for key {}: {}/{} image operations/min (org_id: {})",
                api_key_id, count, limit, auth_key.organization.id.0
            );
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                axum::Json(ErrorResponse::new(
                    format!(
                        "Image generation/edit rate limit exceeded ({count}/{limit} operations/min). Images are rate-limited separately due to high resource usage. Try again in {RATE_LIMIT_WINDOW_SECS} seconds."
                    ),
                    "rate_limit_exceeded".to_string(),
                )),
            ));
        }

        debug!(
            "Image rate limit check passed for {}: {}/{}",
            api_key_id, count, limit
        );
    } else {
        // General rate limit for text operations and other requests
        let (allowed, count, limit) = state.check_limit(api_key_id).await;

        if !allowed {
            warn!(
                "API key rate limit exceeded for key {}: {}/{} requests/min (org_id: {})",
                api_key_id, count, limit, auth_key.organization.id.0
            );
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                axum::Json(ErrorResponse::new(
                    format!(
                        "API rate limit exceeded ({count}/{limit} requests/min). Try again in {RATE_LIMIT_WINDOW_SECS} seconds."
                    ),
                    "rate_limit_exceeded".to_string(),
                )),
            ));
        }

        debug!(
            "API key rate limit check passed for {}: {}/{}",
            api_key_id, count, limit
        );
    }

    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_api_key_rate_limit() {
        let state = RateLimitState::new(5, 10);
        let api_key_id = "test-key-123";

        // First 5 requests should be allowed
        for i in 1..=5 {
            let (allowed, count, limit) = state.check_limit(api_key_id).await;
            assert!(allowed, "Request {i} should be allowed");
            assert_eq!(count, i as u32);
            assert_eq!(limit, 5);
        }

        // 6th request should be denied
        let (allowed, _, _) = state.check_limit(api_key_id).await;
        assert!(!allowed, "Request 6 should be denied");
    }

    #[tokio::test]
    async fn test_different_keys_independent() {
        let state = RateLimitState::new(2, 10);

        let (allowed1, count1, _) = state.check_limit("key-1").await;
        let (allowed2, count2, _) = state.check_limit("key-2").await;

        assert!(allowed1);
        assert!(allowed2);
        assert_eq!(count1, 1);
        assert_eq!(count2, 1);
    }

    #[tokio::test]
    async fn test_image_rate_limit_separate_from_text() {
        let state = RateLimitState::new(100, 3); // 100 text requests, 3 image operations
        let api_key_id = "image-test-key";

        // Text operations should use general limit
        for i in 1..=5 {
            let (allowed, _count, limit) = state.check_limit(api_key_id).await;
            assert!(allowed, "Text request {i} should be allowed (limit is 100)");
            assert_eq!(limit, 100);
        }

        // Image operations should use separate image limit
        for i in 1..=3 {
            let (allowed, count, limit) = state.check_image_limit(api_key_id).await;
            assert!(allowed, "Image operation {i} should be allowed");
            assert_eq!(count, i as u32);
            assert_eq!(limit, 3);
        }

        // 4th image operation should be denied
        let (allowed, _, _) = state.check_image_limit(api_key_id).await;
        assert!(
            !allowed,
            "4th image operation should be denied (limit is 3)"
        );

        // Text operations should still be allowed
        let (allowed, _, _) = state.check_limit(api_key_id).await;
        assert!(
            allowed,
            "Text operations should still be allowed (separate counter)"
        );
    }

    #[test]
    fn test_is_image_operation_detection() {
        // Image generation endpoint
        assert!(is_image_operation("/v1/images/generations", &Method::POST));

        // Image edit endpoint
        assert!(is_image_operation("/v1/images/edits", &Method::POST));

        // Non-image endpoints should not be detected as image operations
        assert!(!is_image_operation("/v1/chat/completions", &Method::POST));
        assert!(!is_image_operation("/v1/responses", &Method::POST));

        // GET requests should not be rate-limited as image operations
        assert!(!is_image_operation("/v1/images/generations", &Method::GET));
        assert!(!is_image_operation("/v1/images/edits", &Method::DELETE));
    }
}
