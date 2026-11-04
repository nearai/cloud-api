// E2E tests for the production-shaped admin router: the routes that moved to
// nearai/cloud-platform (including the database-encryption routes and the
// admin list/detail reads) are not served, while the remaining admin reads
// still are. Uses default `AppBuildOptions` apart from disabling background
// encryption recovery.

use crate::common::*;
use axum::http::{Method, StatusCode};

const ORG: &str = "00000000-0000-0000-0000-000000000001";
const USER: &str = "00000000-0000-0000-0000-000000000002";

/// Every operation that the retired router registers, with path parameters
/// filled in. Keep in sync with `retired` in `build_admin_routes_with_options`.
fn retired_routes() -> Vec<(Method, String)> {
    let routes: Vec<(Method, String)> = vec![
        (Method::PATCH, "/v1/admin/models".into()),
        (Method::POST, "/v1/admin/models/deprecate".into()),
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
            "/v1/admin/models/pricing-changes/some-id".into(),
        ),
        (Method::DELETE, "/v1/admin/models/some-model".into()),
        (
            Method::POST,
            "/v1/admin/models/some-model/deprecation/preview".into(),
        ),
        (
            Method::POST,
            "/v1/admin/models/some-model/deprecation/confirm".into(),
        ),
        (
            Method::PUT,
            format!("/v1/admin/models/some-model/access/{ORG}"),
        ),
        (
            Method::DELETE,
            format!("/v1/admin/models/some-model/access/{ORG}"),
        ),
        (Method::POST, "/v1/admin/services".into()),
        (Method::PATCH, format!("/v1/admin/services/{ORG}")),
        (
            Method::PATCH,
            format!("/v1/admin/organizations/{ORG}/limits"),
        ),
        (
            Method::POST,
            format!("/v1/admin/organizations/{ORG}/staking/farm/sync"),
        ),
        (Method::PATCH, format!("/v1/admin/aml/reports/{ORG}/status")),
        (Method::POST, "/v1/admin/aml/allowlist".into()),
        (
            Method::DELETE,
            "/v1/admin/aml/allowlist/some-account".into(),
        ),
        (
            Method::PATCH,
            format!("/v1/admin/organizations/{ORG}/concurrent-limit"),
        ),
        (
            Method::PATCH,
            format!("/v1/admin/organizations/{ORG}/priority"),
        ),
        (
            Method::PATCH,
            format!("/v1/admin/organizations/{ORG}/fallback"),
        ),
        (Method::PATCH, "/v1/admin/settings/some-key".into()),
        (
            Method::POST,
            format!("/v1/admin/invitation-email-deliveries/{ORG}/resend"),
        ),
        (
            Method::POST,
            format!("/v1/admin/users/{USER}/erasure/preview"),
        ),
        (Method::POST, format!("/v1/admin/users/{USER}/erasure")),
        (Method::POST, "/v1/admin/user-erasures/lookup".into()),
        (
            Method::PUT,
            format!("/v1/admin/organizations/{ORG}/members/{USER}"),
        ),
        (Method::POST, "/v1/admin/access-tokens".into()),
        (Method::GET, "/v1/admin/access-tokens".into()),
        (Method::DELETE, format!("/v1/admin/access-tokens/{ORG}")),
        (Method::PUT, format!("/v1/admin/organizations/{ORG}/policy")),
        (
            Method::POST,
            format!("/v1/admin/organizations/{ORG}/credit/reconciliations"),
        ),
        (
            Method::PUT,
            format!("/v1/admin/organizations/{ORG}/usage-discount"),
        ),
        (Method::POST, "/v1/admin/usage-hourly/recompute".into()),
        // Database encryption.
        (Method::POST, "/v1/admin/database-encryption/scan".into()),
        (Method::POST, "/v1/admin/database-encryption/jobs".into()),
        (
            Method::GET,
            format!("/v1/admin/database-encryption/jobs/{ORG}"),
        ),
        (
            Method::POST,
            format!("/v1/admin/database-encryption/jobs/{ORG}/cancel"),
        ),
        (Method::POST, "/v1/admin/database-encryption/verify".into()),
        // Reads that moved to cloud-platform.
        (Method::GET, "/v1/admin/users".into()),
        (Method::GET, "/v1/admin/api-keys".into()),
        (Method::GET, "/v1/admin/organizations".into()),
        (Method::GET, format!("/v1/admin/organizations/{ORG}")),
        (
            Method::GET,
            format!("/v1/admin/organizations/{ORG}/members"),
        ),
        (Method::GET, "/v1/admin/invitation-email-deliveries".into()),
        (Method::GET, "/v1/admin/aml/reports".into()),
        (Method::GET, "/v1/admin/aml/allowlist".into()),
        (Method::GET, "/v1/admin/feature-requests".into()),
        (Method::GET, "/v1/admin/models/some-model/access".into()),
        (
            Method::GET,
            format!("/v1/admin/organizations/{ORG}/model-access"),
        ),
        (
            Method::GET,
            format!("/v1/admin/organizations/{ORG}/usage/balance"),
        ),
        (
            Method::GET,
            format!("/v1/admin/organizations/{ORG}/staking/farm"),
        ),
        (
            Method::GET,
            format!("/v1/admin/organizations/{ORG}/credit/transfer/reconciliation"),
        ),
        (
            Method::GET,
            format!("/v1/admin/organizations/{ORG}/limits/history"),
        ),
        (Method::GET, format!("/v1/admin/organizations/{ORG}/policy")),
        (
            Method::GET,
            format!("/v1/admin/organizations/{ORG}/usage-discount"),
        ),
    ];
    routes
}

async fn call(
    server: &axum_test::TestServer,
    method: &Method,
    path: &str,
) -> axum_test::TestResponse {
    server
        .method(method.clone(), path)
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({}))
        .await
}

#[tokio::test]
async fn test_retired_admin_routes_are_not_served_in_production_shape() {
    let server = setup_production_shape_test_server().await;

    for (method, path) in retired_routes() {
        let response = call(&server, &method, &path).await;
        assert!(
            matches!(
                response.status_code(),
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ),
            "{method} {path} answered {}, expected 404 or 405",
            response.status_code()
        );
    }
}

#[tokio::test]
async fn test_retained_admin_routes_are_served_in_production_shape() {
    let server = setup_production_shape_test_server().await;

    for path in [
        "/v1/admin/models?limit=1",
        "/v1/admin/models/pricing-changes",
        "/v1/admin/settings",
    ] {
        let response = call(&server, &Method::GET, path).await;
        assert_eq!(response.status_code(), StatusCode::OK, "GET {path}");
    }

    // Environment-dependent reads: only require that the route is mounted.
    for path in [
        "/v1/admin/platform/infra-summary",
        "/v1/admin/platform/metrics",
        "/v1/admin/platform/metrics/timeseries",
        "/v1/admin/platform/billing-summary",
        "/v1/admin/platform/model-revenue",
        "/v1/admin/platform/org-revenue",
        "/v1/admin/platform/model-consumption-timeseries",
        "/v1/admin/platform/performance-timeseries",
        "/v1/admin/platform/revenue-density",
    ] {
        let response = call(&server, &Method::GET, path).await;
        assert!(
            !matches!(
                response.status_code(),
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ),
            "GET {path} answered {}",
            response.status_code()
        );
    }

    // Reads whose handler can answer 404 for an unknown id: a method the path
    // does not register answers 405 only when the path itself is mounted.
    for path in [
        "/v1/admin/models/some-model/history".to_string(),
        "/v1/admin/settings/some-key".to_string(),
        format!("/v1/admin/organizations/{ORG}/concurrent-limit"),
        format!("/v1/admin/organizations/{ORG}/priority"),
        format!("/v1/admin/organizations/{ORG}/fallback"),
        format!("/v1/admin/organizations/{ORG}/metrics"),
        format!("/v1/admin/organizations/{ORG}/metrics/timeseries"),
    ] {
        let response = call(&server, &Method::POST, &path).await;
        assert_eq!(
            response.status_code(),
            StatusCode::METHOD_NOT_ALLOWED,
            "POST {path}"
        );
    }

    // Paths that keep their read also reject the retired method.
    let response = call(&server, &Method::PATCH, "/v1/admin/models").await;
    assert_eq!(response.status_code(), StatusCode::METHOD_NOT_ALLOWED);
}
