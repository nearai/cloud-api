use axum::{
    extract::{Request, State},
    http::StatusCode,
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
        let counter = self
            .key_limits
            .get_with(api_key_id.to_string(), async { Arc::new(Counter::new(0)) })
            .await;

        let count = counter.increment();
        let allowed = count <= self.rate_limit;

        (allowed, count, self.rate_limit)
    }
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
}
