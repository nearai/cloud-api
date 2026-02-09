use axum::{
    body::Body,
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::Response,
};
use http_body_util::BodyExt;
use moka::future::Cache;
use services::models::ModelsServiceTrait;
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

/// Model metadata extracted from request body for rate limiting decisions
#[derive(Clone, Debug)]
pub struct RequestModelMetadata {
    pub model_name: String,
    pub is_image_model: bool,
}

#[derive(Clone)]
pub struct RateLimitState {
    /// General request rate limiter (text operations, non-image endpoints)
    key_limits: Arc<Cache<String, Arc<Counter>>>,
    /// Image-specific rate limiter (image generation, edits - 100x more expensive)
    image_limits: Arc<Cache<String, Arc<Counter>>>,
    /// General rate limit (requests per minute)
    rate_limit: u32,
    /// Image rate limit (image operations per minute)
    image_rate_limit: u32,
    /// Models service for checking model capabilities
    models_service: Arc<dyn ModelsServiceTrait>,
}

impl RateLimitState {
    /// Create a new rate limiter with separate text and image limits
    pub fn new(
        rate_limit: u32,
        image_rate_limit: u32,
        models_service: Arc<dyn ModelsServiceTrait>,
    ) -> Self {
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
            key_limits: Arc::new(key_limits),
            image_limits: Arc::new(image_limits),
            rate_limit,
            image_rate_limit,
            models_service,
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

/// Extract model name from JSON request body
fn extract_model_name_from_body(body_bytes: &[u8]) -> Option<String> {
    let json: serde_json::Value = serde_json::from_slice(body_bytes).ok()?;
    json.get("model")?.as_str().map(String::from)
}

/// Check if model has image generation capability
async fn check_model_image_capability(
    models_service: &Arc<dyn ModelsServiceTrait>,
    model_name: &str,
) -> Option<bool> {
    match models_service.get_model_by_name(model_name).await {
        Ok(model) => {
            let is_image = model
                .output_modalities
                .as_ref()
                .is_some_and(|modalities| modalities.contains(&"image".to_string()));
            Some(is_image)
        }
        Err(_) => {
            // Fail open: if we can't determine model type, allow request
            None
        }
    }
}

/// Determine if a request is image-related based on path and/or model metadata
fn is_image_operation(
    path: &str,
    method: &Method,
    model_metadata: Option<&RequestModelMetadata>,
) -> bool {
    // Only POST requests to image endpoints are rate-limited as image operations
    if *method != Method::POST {
        return false;
    }

    // Direct image endpoints
    if path.contains("/images/generations") || path.contains("/images/edits") {
        return true;
    }

    // /responses with image model
    if path.contains("/responses") {
        if let Some(metadata) = model_metadata {
            return metadata.is_image_model;
        }
    }

    false
}

pub async fn api_key_rate_limit_middleware(
    State(state): State<RateLimitState>,
    mut request: Request,
    next: Next,
) -> Result<Response, (StatusCode, axum::Json<ErrorResponse>)> {
    let auth_key = match request.extensions().get::<AuthenticatedApiKey>() {
        Some(key) => key.clone(),
        None => return Ok(next.run(request).await),
    };

    let api_key_id = &auth_key.api_key.id.0;

    // Determine if this is an image operation by checking path and/or model metadata
    let path = request.uri().path().to_string();
    let method = request.method().clone();

    // For /responses, peek at body to extract model name and check capabilities
    let mut model_metadata: Option<RequestModelMetadata> = None;

    if method == Method::POST && path.contains("/responses") {
        // Buffer request body (similar to body_hash_middleware pattern)
        let (parts, body) = request.into_parts();

        let body_bytes = match body.collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    axum::Json(ErrorResponse::new(
                        "Failed to read request body".to_string(),
                        "invalid_request".to_string(),
                    )),
                ));
            }
        };

        // Try to extract model name and check if it's an image model
        if let Some(model_name) = extract_model_name_from_body(&body_bytes) {
            if let Some(is_image) =
                check_model_image_capability(&state.models_service, &model_name).await
            {
                model_metadata = Some(RequestModelMetadata {
                    model_name,
                    is_image_model: is_image,
                });
            }
        }

        // Reconstruct request with buffered body
        request = Request::from_parts(parts, Body::from(body_bytes));

        // Cache metadata in extensions for potential downstream use
        if let Some(ref metadata) = model_metadata {
            request.extensions_mut().insert(metadata.clone());
        }
    }

    // Determine if this is an image operation
    let is_image = is_image_operation(&path, &method, model_metadata.as_ref());

    // Apply appropriate rate limit
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

    /// Mock implementation of ModelsServiceTrait for testing
    struct MockModelsService;

    #[async_trait::async_trait]
    impl ModelsServiceTrait for MockModelsService {
        async fn get_models(
            &self,
        ) -> Result<Vec<services::models::ModelInfo>, services::models::ModelsError> {
            Ok(vec![])
        }

        async fn get_models_with_pricing(
            &self,
            _limit: i64,
            _offset: i64,
        ) -> Result<(Vec<services::models::ModelWithPricing>, i64), services::models::ModelsError>
        {
            Ok((vec![], 0))
        }

        async fn get_model_by_name(
            &self,
            _model_name: &str,
        ) -> Result<services::models::ModelWithPricing, services::models::ModelsError> {
            Err(services::models::ModelsError::NotFound(
                _model_name.to_string(),
            ))
        }

        async fn resolve_and_get_model(
            &self,
            _identifier: &str,
        ) -> Result<services::models::ModelWithPricing, services::models::ModelsError> {
            Err(services::models::ModelsError::NotFound("test".to_string()))
        }

        async fn get_configured_model_names(
            &self,
        ) -> Result<Vec<String>, services::models::ModelsError> {
            Ok(vec![])
        }
    }

    fn create_mock_state(rate_limit: u32, image_rate_limit: u32) -> RateLimitState {
        RateLimitState::new(rate_limit, image_rate_limit, Arc::new(MockModelsService))
    }

    #[tokio::test]
    async fn test_api_key_rate_limit() {
        let state = create_mock_state(5, 10);
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
        let state = create_mock_state(2, 10);

        let (allowed1, count1, _) = state.check_limit("key-1").await;
        let (allowed2, count2, _) = state.check_limit("key-2").await;

        assert!(allowed1);
        assert!(allowed2);
        assert_eq!(count1, 1);
        assert_eq!(count2, 1);
    }

    #[tokio::test]
    async fn test_image_rate_limit_separate_from_text() {
        let state = create_mock_state(100, 3); // 100 text requests, 3 image operations
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
        assert!(is_image_operation(
            "/v1/images/generations",
            &Method::POST,
            None
        ));

        // Image edit endpoint
        assert!(is_image_operation("/v1/images/edits", &Method::POST, None));

        // Non-image endpoints should not be detected as image operations
        assert!(!is_image_operation(
            "/v1/chat/completions",
            &Method::POST,
            None
        ));

        // /v1/responses without model metadata should not be detected as image
        assert!(!is_image_operation("/v1/responses", &Method::POST, None));

        // /v1/responses with image model metadata should be detected
        let image_metadata = RequestModelMetadata {
            model_name: "flux-schnell".to_string(),
            is_image_model: true,
        };
        assert!(is_image_operation(
            "/v1/responses",
            &Method::POST,
            Some(&image_metadata)
        ));

        // /v1/responses with text model metadata should not be detected as image
        let text_metadata = RequestModelMetadata {
            model_name: "gpt-4".to_string(),
            is_image_model: false,
        };
        assert!(!is_image_operation(
            "/v1/responses",
            &Method::POST,
            Some(&text_metadata)
        ));

        // GET requests should not be rate-limited as image operations
        assert!(!is_image_operation(
            "/v1/images/generations",
            &Method::GET,
            None
        ));
        assert!(!is_image_operation(
            "/v1/images/edits",
            &Method::DELETE,
            None
        ));
    }
}
