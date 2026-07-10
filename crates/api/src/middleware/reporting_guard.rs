use super::AuthenticatedReportingToken;
use crate::models::ErrorResponse;
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
    Json,
};
use moka::future::Cache;
use std::{
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

const RATE_WINDOW: Duration = Duration::from_secs(60);
const TOKEN_CACHE_CAPACITY: u64 = 50_000;

#[derive(Debug, Clone, Copy)]
pub struct ReportingRequestDeadline(Instant);

impl ReportingRequestDeadline {
    pub const fn instant(self) -> Instant {
        self.0
    }
}

#[derive(Debug)]
struct Counter(AtomicU32);

impl Counter {
    const fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    fn increment(&self) -> u32 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }
}

#[derive(Clone)]
pub struct ReportingGuardState {
    global_requests: Cache<(), Arc<Counter>>,
    token_requests: Cache<Uuid, Arc<Counter>>,
    global_concurrency: Arc<Semaphore>,
    token_concurrency: Cache<Uuid, Arc<Semaphore>>,
    global_requests_per_minute: u32,
    token_requests_per_minute: u32,
    token_max_concurrent_requests: usize,
    request_timeout: Duration,
}

impl ReportingGuardState {
    pub fn from_config(config: &config::UsageReportingConfig) -> Self {
        Self::new(
            config.global_requests_per_minute,
            config.token_requests_per_minute,
            config.max_concurrent_requests,
            config.token_max_concurrent_requests,
            Duration::from_secs(config.request_timeout_seconds),
        )
    }

    pub fn new(
        global_requests_per_minute: u32,
        token_requests_per_minute: u32,
        max_concurrent_requests: usize,
        token_max_concurrent_requests: usize,
        request_timeout: Duration,
    ) -> Self {
        Self {
            global_requests: Cache::builder()
                .time_to_live(RATE_WINDOW)
                .max_capacity(1)
                .build(),
            token_requests: Cache::builder()
                .time_to_live(RATE_WINDOW)
                .max_capacity(TOKEN_CACHE_CAPACITY)
                .build(),
            global_concurrency: Arc::new(Semaphore::new(max_concurrent_requests)),
            token_concurrency: Cache::builder().max_capacity(TOKEN_CACHE_CAPACITY).build(),
            global_requests_per_minute,
            token_requests_per_minute,
            token_max_concurrent_requests,
            request_timeout,
        }
    }

    async fn check_global_limit(&self) -> bool {
        let counter = self
            .global_requests
            .get_with((), async { Arc::new(Counter::new()) })
            .await;
        counter.increment() <= self.global_requests_per_minute
    }

    async fn check_token_limit(&self, token_id: Uuid) -> bool {
        let counter = self
            .token_requests
            .get_with(token_id, async { Arc::new(Counter::new()) })
            .await;
        counter.increment() <= self.token_requests_per_minute
    }

    fn try_acquire(&self) -> Option<OwnedSemaphorePermit> {
        self.global_concurrency.clone().try_acquire_owned().ok()
    }

    async fn try_acquire_token(&self, token_id: Uuid) -> Option<OwnedSemaphorePermit> {
        self.token_concurrency
            .get_with(token_id, async {
                Arc::new(Semaphore::new(self.token_max_concurrent_requests))
            })
            .await
            .try_acquire_owned()
            .ok()
    }
}

type GuardError = (StatusCode, Json<ErrorResponse>);

pub async fn reporting_global_guard_middleware(
    State(state): State<ReportingGuardState>,
    mut request: Request,
    next: Next,
) -> Result<Response, GuardError> {
    if !state.check_global_limit().await {
        return Err(rate_limit_error(
            "Programmatic usage reporting rate limit exceeded. Retry later.",
        ));
    }
    let _permit = state.try_acquire().ok_or_else(|| {
        rate_limit_error("Too many reporting requests are in progress. Retry shortly.")
    })?;

    let deadline = Instant::now() + state.request_timeout;
    request
        .extensions_mut()
        .insert(ReportingRequestDeadline(deadline));
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), next.run(request))
        .await
        .map_err(|_| {
            (
                StatusCode::GATEWAY_TIMEOUT,
                Json(ErrorResponse::new(
                    "Usage reporting request timed out".to_string(),
                    "reporting_request_timeout".to_string(),
                )),
            )
        })
}

pub async fn reporting_token_guard_middleware(
    State(state): State<ReportingGuardState>,
    request: Request,
    next: Next,
) -> Result<Response, GuardError> {
    let token_id = request
        .extensions()
        .get::<AuthenticatedReportingToken>()
        .map(|token| token.id)
        .ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Reporting authentication context is missing".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;
    let _permit = state.try_acquire_token(token_id).await.ok_or_else(|| {
        rate_limit_error("Too many concurrent requests for this reporting token.")
    })?;
    if !state.check_token_limit(token_id).await {
        return Err(rate_limit_error(
            "Reporting token rate limit exceeded. Retry later.",
        ));
    }

    Ok(next.run(request).await)
}

fn rate_limit_error(message: &str) -> GuardError {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(ErrorResponse::new(
            message.to_string(),
            "rate_limit_exceeded".to_string(),
        )),
    )
}

#[cfg(test)]
#[path = "reporting_guard_tests.rs"]
mod tests;
