// E2E tests: routes that moved to nearai/cloud-platform are not served by cloud-api.
// Each entry is a route this branch deletes. Later removals extend the table.

use crate::common::*;
use axum::http::{Method, StatusCode};

const ORG: &str = "00000000-0000-0000-0000-000000000001";
const ID: &str = "00000000-0000-0000-0000-000000000002";

fn retired_routes() -> Vec<(Method, String)> {
    let org = format!("/v1/organizations/{ORG}");
    let admin_org = format!("/v1/admin/organizations/{ORG}");
    vec![
        // Reporting tokens
        (Method::GET, format!("{org}/reporting-tokens")),
        (Method::POST, format!("{org}/reporting-tokens")),
        (Method::DELETE, format!("{org}/reporting-tokens/{ID}")),
        (
            Method::GET,
            format!("{org}/usage/reporting-token-auth-probe"),
        ),
        (Method::GET, format!("{org}/usage/export")),
        (Method::GET, format!("{org}/usage/summary")),
        // Usage console
        (Method::GET, format!("{org}/usage/balance")),
        (Method::GET, format!("{org}/usage/history")),
        (Method::GET, format!("{org}/usage/by-model")),
        (Method::GET, format!("{org}/usage/metrics")),
        (Method::GET, format!("{org}/usage/timeseries")),
        (Method::GET, format!("{org}/service-usage/history")),
        (
            Method::GET,
            format!("/v1/workspaces/{ORG}/api-keys/{ID}/usage/history"),
        ),
        // Billing and credit
        (Method::POST, "/v1/billing/costs".into()),
        (Method::POST, format!("{org}/credit/transfer")),
        (Method::GET, format!("{org}/credit/transfer/capacity")),
        (Method::GET, format!("{org}/credit/transfers")),
        (Method::GET, format!("{org}/credit/transfers/{ID}")),
        (Method::GET, format!("{org}/credentials")),
        (Method::POST, format!("{org}/credentials")),
        (Method::PUT, format!("{org}/credentials/{ID}")),
        (Method::DELETE, format!("{org}/credentials/{ID}")),
        (Method::POST, format!("{org}/credentials/{ID}/rotate")),
        (Method::GET, format!("{admin_org}/policy")),
        (Method::PUT, format!("{admin_org}/policy")),
        (Method::GET, format!("{admin_org}/usage/balance")),
        (Method::GET, format!("{admin_org}/usage-discount")),
        (Method::PUT, format!("{admin_org}/usage-discount")),
        (Method::POST, format!("{admin_org}/credit/reconciliations")),
        (
            Method::GET,
            format!("{admin_org}/credit/transfer/reconciliation"),
        ),
        (Method::POST, "/v1/admin/usage-hourly/recompute".into()),
        (Method::GET, "/v1/staking/farm/config".into()),
        (Method::GET, format!("{org}/staking/farm")),
        (Method::POST, format!("{org}/staking/farm/bind")),
        (Method::POST, format!("{org}/staking/farm/sync")),
        (Method::GET, format!("{admin_org}/staking/farm")),
        (Method::POST, format!("{admin_org}/staking/farm/sync")),
        // Analytics, platform reports and directory reads
        (Method::GET, format!("{admin_org}/metrics")),
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
        (Method::GET, "/v1/admin/users".into()),
        (Method::GET, "/v1/admin/organizations".into()),
        (Method::GET, admin_org.clone()),
        (Method::GET, format!("{admin_org}/members")),
        (Method::GET, "/v1/admin/api-keys".into()),
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
