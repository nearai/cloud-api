use axum::{
    extract::{Request, State},
    http::{header::RETRY_AFTER, HeaderName, HeaderValue, StatusCode},
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
use crate::models::AnthropicErrorResponse;
use crate::models::ErrorResponse;

const DEFAULT_API_KEY_RATE_LIMIT: u32 = 1000; // requests per minute
const RATE_LIMIT_WINDOW_SECS: u64 = 60;
const RATE_LIMIT_CACHE_MAX_CAPACITY: u64 = 50_000;
const ANTHROPIC_COUNT_TOKENS_SCOPE: &str = "anthropic_count_tokens";
const ANTHROPIC_COUNT_TOKENS_RATE_LIMIT: u32 = 100;

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
    key_limits: Cache<String, Arc<Counter>>,
    rate_limit: u32,
}

impl Default for RateLimitState {
    fn default() -> Self {
        Self::new(DEFAULT_API_KEY_RATE_LIMIT)
    }
}

impl RateLimitState {
    pub fn new(rate_limit: u32) -> Self {
        let window = Duration::from_secs(RATE_LIMIT_WINDOW_SECS);

        let key_limits: Cache<String, Arc<Counter>> = Cache::builder()
            .time_to_live(window)
            .max_capacity(RATE_LIMIT_CACHE_MAX_CAPACITY)
            .build();

        Self {
            key_limits,
            rate_limit,
        }
    }

    async fn check_limit(&self, api_key_id: &str) -> (bool, u32, u32) {
        self.check_limit_with_limit(api_key_id, self.rate_limit)
            .await
    }

    async fn check_limit_with_limit(&self, api_key_id: &str, limit: u32) -> (bool, u32, u32) {
        let counter = self
            .key_limits
            .get_with(api_key_id.to_string(), async { Arc::new(Counter::new(0)) })
            .await;

        let count = counter.increment();
        let allowed = count <= limit;

        (allowed, count, limit)
    }
}

/// 429 rejection from the per-key limiter: status, `Retry-After` header, body.
/// The header value matches the fixed rate-limit window (and the "Try again in
/// N seconds" prose in the message) so SDK backoff waits out the window.
pub type RateLimitedResponse = (
    StatusCode,
    [(HeaderName, HeaderValue); 1],
    axum::Json<ErrorResponse>,
);

fn rate_limited_response(count: u32, limit: u32) -> RateLimitedResponse {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(RETRY_AFTER, HeaderValue::from(RATE_LIMIT_WINDOW_SECS))],
        axum::Json(ErrorResponse::new(
            format!(
                "API rate limit exceeded ({count}/{limit} requests/min). Try again in {RATE_LIMIT_WINDOW_SECS} seconds."
            ),
            "rate_limit_exceeded".to_string(),
        )),
    )
}

pub async fn check_rate_limit_for_api_key(
    state: &RateLimitState,
    auth_key: &AuthenticatedApiKey,
) -> Result<(), RateLimitedResponse> {
    let api_key_id = &auth_key.api_key.id.0;
    let (allowed, count, limit) = state.check_limit(api_key_id).await;

    if !allowed {
        warn!(
            "API key rate limit exceeded for key {}: {}/{} requests/min (org_id: {})",
            api_key_id, count, limit, auth_key.organization.id.0
        );
        return Err(rate_limited_response(count, limit));
    }

    debug!(
        "API key rate limit check passed for {}: {}/{}",
        api_key_id, count, limit
    );
    Ok(())
}

async fn check_rate_limit_for_api_key_in_scope(
    state: &RateLimitState,
    auth_key: &AuthenticatedApiKey,
    scope: &str,
    limit: u32,
) -> Result<(), RateLimitedResponse> {
    let api_key_id = &auth_key.api_key.id.0;
    let bucket = format!("{scope}:{api_key_id}");
    let (allowed, count, limit) = state.check_limit_with_limit(&bucket, limit).await;

    if !allowed {
        warn!(
            "Scoped API key rate limit exceeded for key {}: {}/{} requests/min (org_id: {}, scope: {})",
            api_key_id, count, limit, auth_key.organization.id.0, scope
        );
        return Err(rate_limited_response(count, limit));
    }

    debug!(
        "Scoped API key rate limit check passed for {}: {}/{} (scope: {})",
        api_key_id, count, limit, scope
    );
    Ok(())
}

pub async fn api_key_rate_limit_middleware(
    State(state): State<RateLimitState>,
    request: Request,
    next: Next,
) -> Result<Response, RateLimitedResponse> {
    let auth_key = match request.extensions().get::<AuthenticatedApiKey>() {
        Some(key) => key.clone(),
        None => return Ok(next.run(request).await),
    };

    check_rate_limit_for_api_key(&state, &auth_key).await?;
    Ok(next.run(request).await)
}

pub async fn anthropic_api_key_rate_limit_middleware(
    State(state): State<RateLimitState>,
    request: Request,
    next: Next,
) -> Result<
    Response,
    (
        StatusCode,
        [(HeaderName, HeaderValue); 1],
        axum::Json<AnthropicErrorResponse>,
    ),
> {
    let Some(auth_key) = request.extensions().get::<AuthenticatedApiKey>().cloned() else {
        return Ok(next.run(request).await);
    };

    if let Err((status, headers, axum::Json(error))) =
        check_rate_limit_for_api_key(&state, &auth_key).await
    {
        return Err((
            status,
            headers,
            axum::Json(AnthropicErrorResponse::new(
                "rate_limit_error",
                error.error.message,
            )),
        ));
    }

    Ok(next.run(request).await)
}

/// Anthropic rate-limits token counting separately from Messages creation.
/// Keep a distinct per-key bucket so free count requests cannot consume the
/// request allowance used by billable inference.
pub async fn anthropic_count_tokens_rate_limit_middleware(
    State(state): State<RateLimitState>,
    request: Request,
    next: Next,
) -> Result<
    Response,
    (
        StatusCode,
        [(HeaderName, HeaderValue); 1],
        axum::Json<AnthropicErrorResponse>,
    ),
> {
    let Some(auth_key) = request.extensions().get::<AuthenticatedApiKey>().cloned() else {
        return Ok(next.run(request).await);
    };

    if let Err((status, headers, axum::Json(error))) = check_rate_limit_for_api_key_in_scope(
        &state,
        &auth_key,
        ANTHROPIC_COUNT_TOKENS_SCOPE,
        ANTHROPIC_COUNT_TOKENS_RATE_LIMIT,
    )
    .await
    {
        return Err((
            status,
            headers,
            axum::Json(AnthropicErrorResponse::new(
                "rate_limit_error",
                error.error.message,
            )),
        ));
    }

    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_api_key_rate_limit() {
        let state = RateLimitState::new(5);
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
        let state = RateLimitState::new(2);

        let (allowed1, count1, _) = state.check_limit("key-1").await;
        let (allowed2, count2, _) = state.check_limit("key-2").await;

        assert!(allowed1);
        assert!(allowed2);
        assert_eq!(count1, 1);
        assert_eq!(count2, 1);
    }

    #[tokio::test]
    async fn count_tokens_uses_a_bucket_independent_from_inference() {
        let state = RateLimitState::new(1);
        let api_key_id = "test-key-123";
        let count_tokens_bucket = format!("{ANTHROPIC_COUNT_TOKENS_SCOPE}:{api_key_id}");

        assert!(state.check_limit(api_key_id).await.0);
        let (_, _, limit) = state
            .check_limit_with_limit(&count_tokens_bucket, ANTHROPIC_COUNT_TOKENS_RATE_LIMIT)
            .await;
        assert_eq!(limit, ANTHROPIC_COUNT_TOKENS_RATE_LIMIT);
        assert!(!state.check_limit(api_key_id).await.0);
        for _ in 1..ANTHROPIC_COUNT_TOKENS_RATE_LIMIT {
            assert!(
                state
                    .check_limit_with_limit(
                        &count_tokens_bucket,
                        ANTHROPIC_COUNT_TOKENS_RATE_LIMIT,
                    )
                    .await
                    .0
            );
        }
        assert!(
            !state
                .check_limit_with_limit(&count_tokens_bucket, ANTHROPIC_COUNT_TOKENS_RATE_LIMIT,)
                .await
                .0
        );
    }

    #[test]
    fn test_rate_limited_response_carries_retry_after() {
        use axum::response::IntoResponse;

        let response = rate_limited_response(1001, 1000).into_response();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        // SDK backoff honors Retry-After; the value must match the fixed
        // window advertised in the error message ("Try again in 60 seconds").
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        assert_eq!(retry_after, Some(RATE_LIMIT_WINDOW_SECS));
    }
}
