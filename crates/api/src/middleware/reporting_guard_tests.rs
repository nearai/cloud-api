use super::{
    reporting_global_guard_middleware, reporting_token_guard_middleware, ReportingGuardState,
};
use crate::{middleware::AuthenticatedReportingToken, models::ErrorResponse};
use axum::{
    body::Body,
    http::{Request, StatusCode},
    middleware::from_fn_with_state,
    routing::get,
    Extension, Router,
};
use http_body_util::BodyExt as _;
use services::reporting_tokens::REPORTING_TOKEN_SCOPE_USAGE_READ;
use std::time::Duration;
use tower::ServiceExt as _;
use uuid::Uuid;

#[tokio::test]
async fn global_reporting_limit_caps_all_credentials() {
    let state = ReportingGuardState::new(2, 10, 1, 1, Duration::from_secs(1));

    assert!(state.check_global_limit().await);
    assert!(state.check_global_limit().await);
    assert!(!state.check_global_limit().await);
}

#[tokio::test]
async fn reporting_token_limits_are_independent() {
    let state = ReportingGuardState::new(10, 1, 1, 1, Duration::from_secs(1));
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();

    assert!(state.check_token_limit(first).await);
    assert!(!state.check_token_limit(first).await);
    assert!(state.check_token_limit(second).await);
}

#[tokio::test]
async fn reporting_concurrency_is_fail_fast() {
    let state = ReportingGuardState::new(10, 10, 1, 1, Duration::from_secs(1));
    let permit = state.try_acquire().expect("first request should acquire");

    assert!(state.try_acquire().is_none());
    drop(permit);
    assert!(state.try_acquire().is_some());
}

#[tokio::test]
async fn per_token_concurrency_is_fail_fast() {
    let state = ReportingGuardState::new(10, 10, 2, 1, Duration::from_secs(1));
    let token_id = Uuid::new_v4();
    let permit = state
        .try_acquire_token(token_id)
        .await
        .expect("first request should acquire");

    assert!(state.try_acquire_token(token_id).await.is_none());
    drop(permit);
    assert!(state.try_acquire_token(token_id).await.is_some());
}

#[tokio::test]
async fn token_guard_runs_after_auth_context_and_returns_json_429() {
    let token_id = Uuid::new_v4();
    let token = AuthenticatedReportingToken {
        id: token_id,
        organization_id: Uuid::new_v4(),
        token_prefix: "rpt-example".to_string(),
        scope: REPORTING_TOKEN_SCOPE_USAGE_READ,
    };
    let state = ReportingGuardState::new(10, 1, 2, 1, Duration::from_secs(1));
    let router = Router::new()
        .route("/", get(|| async { StatusCode::OK }))
        .layer(from_fn_with_state(state, reporting_token_guard_middleware))
        .layer(Extension(token));

    let first = router
        .clone()
        .oneshot(Request::new(Body::empty()))
        .await
        .unwrap();
    let second = router.oneshot(Request::new(Body::empty())).await.unwrap();

    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = second.into_body().collect().await.unwrap().to_bytes();
    let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(error.error.r#type, "rate_limit_exceeded");
}

#[tokio::test]
async fn token_guard_rejects_missing_auth_context() {
    let state = ReportingGuardState::new(10, 10, 2, 1, Duration::from_secs(1));
    let router = Router::new()
        .route("/", get(|| async { StatusCode::OK }))
        .layer(from_fn_with_state(state, reporting_token_guard_middleware));

    let response = router.oneshot(Request::new(Body::empty())).await.unwrap();

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn global_guard_times_out_stalled_requests() {
    let state = ReportingGuardState::new(10, 10, 1, 1, Duration::ZERO);
    let router = Router::new()
        .route(
            "/",
            get(|| async { std::future::pending::<StatusCode>().await }),
        )
        .layer(from_fn_with_state(state, reporting_global_guard_middleware));

    let response = router.oneshot(Request::new(Body::empty())).await.unwrap();

    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
}
