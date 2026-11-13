// E2E tests: routes removed from cloud-api are not served.
// Each entry is a route that was deleted. Later removals extend the table.

use crate::common::*;
use axum::http::{Method, StatusCode};

const ORG: &str = "00000000-0000-0000-0000-000000000001";

fn retired_routes() -> Vec<(Method, String)> {
    let admin_org = format!("/v1/admin/organizations/{ORG}");
    vec![
        // Usage discount admin route
        (Method::GET, format!("{admin_org}/usage-discount")),
        (Method::PUT, format!("{admin_org}/usage-discount")),
        // Organization metrics timeseries and platform reports
        (Method::GET, format!("{admin_org}/metrics/timeseries")),
        (Method::GET, "/v1/admin/platform/metrics".into()),
        (Method::GET, "/v1/admin/platform/metrics/timeseries".into()),
        (Method::GET, "/v1/admin/platform/billing-summary".into()),
        (Method::GET, "/v1/admin/platform/model-revenue".into()),
        (Method::GET, "/v1/admin/platform/org-revenue".into()),
        (Method::GET, "/v1/admin/platform/revenue-density".into()),
        (
            Method::GET,
            "/v1/admin/platform/model-consumption-timeseries".into(),
        ),
        (
            Method::GET,
            "/v1/admin/platform/performance-timeseries".into(),
        ),
        // Directory reads
        (Method::GET, "/v1/admin/users".into()),
        (Method::GET, "/v1/admin/organizations".into()),
        (Method::GET, admin_org.clone()),
    ]
}

#[tokio::test]
async fn retired_routes_are_not_served() {
    let (_server, router) = setup_test_server_and_router().await;
    let session = get_session_id();
    for (method, path) in retired_routes() {
        let response = tower::ServiceExt::oneshot(
            router.clone(),
            axum::http::Request::builder()
                .method(method.clone())
                .uri(&path)
                .header("Authorization", format!("Bearer {session}"))
                .header("Content-Type", "application/json")
                .body(axum::body::Body::from("{}"))
                .expect("test request"),
        )
        .await
        .expect("router response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
    }
}
