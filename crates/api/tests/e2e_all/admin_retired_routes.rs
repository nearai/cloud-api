// E2E tests: routes removed from cloud-api are not served.
// Each entry is a route that was deleted. Later removals extend the table.

use crate::common::*;
use axum::http::{Method, StatusCode};

const ORG: &str = "00000000-0000-0000-0000-000000000001";
const ID: &str = "00000000-0000-0000-0000-000000000002";

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
        // Catalog writes, AML, settings, organization limits and feature requests
        (Method::GET, "/v1/admin/models".into()),
        (Method::PATCH, "/v1/admin/models".into()),
        (Method::POST, "/v1/admin/models/deprecate".into()),
        (Method::GET, "/v1/admin/models/pricing-changes".into()),
        (
            Method::POST,
            "/v1/admin/models/pricing-changes/preview".into(),
        ),
        (
            Method::POST,
            "/v1/admin/models/pricing-changes/confirm".into(),
        ),
        (
            Method::DELETE,
            format!("/v1/admin/models/pricing-changes/{ID}"),
        ),
        (Method::DELETE, "/v1/admin/models/some-model".into()),
        (Method::GET, "/v1/admin/models/some-model/history".into()),
        (
            Method::POST,
            "/v1/admin/models/some-model/deprecation/preview".into(),
        ),
        (
            Method::POST,
            "/v1/admin/models/some-model/deprecation/confirm".into(),
        ),
        (Method::GET, "/v1/admin/models/some-model/access".into()),
        (
            Method::PUT,
            format!("/v1/admin/models/some-model/access/{ORG}"),
        ),
        (
            Method::DELETE,
            format!("/v1/admin/models/some-model/access/{ORG}"),
        ),
        (Method::GET, format!("{admin_org}/model-access")),
        (Method::POST, "/v1/admin/services".into()),
        (Method::PATCH, format!("/v1/admin/services/{ID}")),
        (Method::PATCH, format!("{admin_org}/limits")),
        (Method::GET, format!("{admin_org}/limits/history")),
        (Method::GET, format!("{admin_org}/concurrent-limit")),
        (Method::PATCH, format!("{admin_org}/concurrent-limit")),
        (Method::GET, format!("{admin_org}/priority")),
        (Method::PATCH, format!("{admin_org}/priority")),
        (Method::GET, format!("{admin_org}/fallback")),
        (Method::PATCH, format!("{admin_org}/fallback")),
        (Method::GET, "/v1/admin/aml/reports".into()),
        (Method::PATCH, format!("/v1/admin/aml/reports/{ID}/status")),
        (Method::GET, "/v1/admin/aml/allowlist".into()),
        (Method::POST, "/v1/admin/aml/allowlist".into()),
        (
            Method::DELETE,
            "/v1/admin/aml/allowlist/some-account".into(),
        ),
        (Method::GET, "/v1/admin/settings".into()),
        (Method::GET, "/v1/admin/settings/placement".into()),
        (Method::PATCH, "/v1/admin/settings/placement".into()),
        (Method::POST, "/v1/feature-requests".into()),
        (Method::GET, "/v1/admin/feature-requests".into()),
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
