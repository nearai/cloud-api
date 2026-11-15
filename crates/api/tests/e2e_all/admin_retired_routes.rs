// E2E tests: routes removed from cloud-api are not served.
// Each entry is a route that was deleted. Later removals extend the table.

use crate::common::*;
use axum::http::{Method, StatusCode};

const ORG: &str = "00000000-0000-0000-0000-000000000001";
const ID: &str = "00000000-0000-0000-0000-000000000002";

fn retired_routes() -> Vec<(Method, String)> {
    let admin_org = format!("/v1/admin/organizations/{ORG}");
    let org = format!("/v1/organizations/{ORG}");
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
        // Reporting tokens and the usage reporting routes they authorized
        (Method::GET, format!("{org}/reporting-tokens")),
        (Method::POST, format!("{org}/reporting-tokens")),
        (Method::DELETE, format!("{org}/reporting-tokens/{ID}")),
        (
            Method::GET,
            format!("{org}/usage/reporting-token-auth-probe"),
        ),
        (Method::GET, format!("{org}/usage/export")),
        (Method::GET, format!("{org}/usage/summary")),
        // Service usage history
        (Method::GET, format!("{org}/service-usage/history")),
        // Admin balance, member list and API key list
        (Method::GET, format!("{admin_org}/usage/balance")),
        (Method::GET, format!("{admin_org}/members")),
        (Method::GET, "/v1/admin/api-keys".into()),
        // Credit transfer detail, reconciliation and policy
        (Method::GET, format!("{org}/credit/transfers/{ID}")),
        (
            Method::GET,
            format!("{admin_org}/credit/transfer/reconciliation"),
        ),
        (Method::POST, format!("{admin_org}/credit/reconciliations")),
        (Method::GET, format!("{admin_org}/policy")),
        (Method::PUT, format!("{admin_org}/policy")),
        // Organization credentials
        (Method::POST, format!("{org}/credentials")),
        (Method::GET, format!("{org}/credentials")),
        (Method::PUT, format!("{org}/credentials/{ID}")),
        (Method::DELETE, format!("{org}/credentials/{ID}")),
        (Method::POST, format!("{org}/credentials/{ID}/rotate")),
        // Hourly usage recompute
        (Method::POST, "/v1/admin/usage-hourly/recompute".into()),
        // Staking farm configuration, state, binding and admin reads
        (Method::GET, "/v1/staking/farm/config".into()),
        (Method::GET, format!("{org}/staking/farm")),
        (Method::POST, format!("{org}/staking/farm/bind")),
        (Method::GET, format!("{admin_org}/staking/farm")),
        (Method::POST, format!("{admin_org}/staking/farm/sync")),
        // Organization read, update, delete and settings
        (Method::GET, org.clone()),
        (Method::PUT, org.clone()),
        (Method::DELETE, org.clone()),
        (Method::GET, format!("{org}/settings")),
        (Method::PATCH, format!("{org}/settings")),
        // Organization invitations and member changes
        (Method::POST, format!("{org}/members/invite-by-email")),
        (Method::GET, format!("{org}/members/invitations")),
        (Method::DELETE, format!("{org}/members/invitations/{ID}")),
        (Method::DELETE, format!("{org}/invitations/{ID}")),
        (Method::PUT, format!("{org}/members/{ID}")),
        (Method::DELETE, format!("{org}/members/{ID}")),
        // Current user status, profile, contact email, invitations and tokens
        (Method::GET, "/v1/users/me/status".into()),
        (Method::PUT, "/v1/users/me/profile".into()),
        (Method::POST, "/v1/users/me/contact-email".into()),
        (Method::DELETE, "/v1/users/me/contact-email".into()),
        (Method::GET, "/v1/users/me/invitations".into()),
        (
            Method::POST,
            format!("/v1/users/me/invitations/{ID}/accept"),
        ),
        (
            Method::POST,
            format!("/v1/users/me/invitations/{ID}/decline"),
        ),
        (Method::DELETE, "/v1/users/me/tokens".into()),
        (Method::POST, "/v1/users/contact-email/verify".into()),
        // Token-based invitations
        (Method::GET, format!("/v1/invitations/{ID}")),
        (Method::POST, format!("/v1/invitations/{ID}/accept")),
        // Workspace read, update and delete
        (Method::GET, format!("/v1/workspaces/{ID}")),
        (Method::PUT, format!("/v1/workspaces/{ID}")),
        (Method::DELETE, format!("/v1/workspaces/{ID}")),
        // Admin invitation delivery, user erasure, member role and access tokens
        (Method::GET, "/v1/admin/invitation-email-deliveries".into()),
        (
            Method::POST,
            format!("/v1/admin/invitation-email-deliveries/{ID}/resend"),
        ),
        (
            Method::POST,
            format!("/v1/admin/users/{ID}/erasure/preview"),
        ),
        (Method::POST, format!("/v1/admin/users/{ID}/erasure")),
        (Method::POST, "/v1/admin/user-erasures/lookup".into()),
        (Method::PUT, format!("{admin_org}/members/{ID}")),
        (Method::POST, "/v1/admin/access-tokens".into()),
        (Method::GET, "/v1/admin/access-tokens".into()),
        (Method::DELETE, format!("/v1/admin/access-tokens/{ID}")),
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
        // The generic fallback body proves no handler matched; a mounted handler that
        // reports "unknown model/org" would also return 404 with a different body.
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(
            body["error"]["message"], "Not found",
            "{method} {path} was not served by the fallback: {body}"
        );
    }
}
