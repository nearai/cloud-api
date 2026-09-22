use crate::common::*;
use services::admin::OrganizationMetrics;

async fn setup_hygiene_server() -> axum_test::TestServer {
    std::env::set_var("DEV", "1");
    std::env::set_var("BRAVE_SEARCH_PRO_API_KEY", "analytics-range-hygiene-test");
    setup_test_server().await
}

async fn admin_get(server: &axum_test::TestServer, path: &str) -> axum_test::TestResponse {
    server
        .get(path)
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await
}

fn assert_invalid_parameter(response: axum_test::TestResponse) {
    assert_eq!(response.status_code(), 400, "{}", response.text());
    let error = response.json::<api::models::ErrorResponse>();
    assert_eq!(error.error.r#type, "invalid_parameter");
}

#[tokio::test]
async fn revenue_density_rejects_malformed_bounds() {
    let server = setup_hygiene_server().await;

    for query in ["start=not-a-timestamp", "end=not-a-timestamp"] {
        assert_invalid_parameter(
            admin_get(
                &server,
                &format!("/v1/admin/platform/revenue-density?{query}"),
            )
            .await,
        );
    }
}

#[tokio::test]
async fn revenue_density_rejects_reversed_bounds() {
    let server = setup_hygiene_server().await;

    assert_invalid_parameter(
        admin_get(
            &server,
            "/v1/admin/platform/revenue-density?start=2026-09-22T00:00:00Z&end=2026-09-21T00:00:00Z",
        )
        .await,
    );

    assert_invalid_parameter(
        admin_get(
            &server,
            "/v1/admin/platform/revenue-density?start=2026-09-22T00:00:00Z&end=2026-09-22T00:00:00Z",
        )
        .await,
    );
}

#[tokio::test]
async fn revenue_density_enforces_90_day_cap() {
    let server = setup_hygiene_server().await;

    let at_cap = admin_get(
        &server,
        "/v1/admin/platform/revenue-density?start=2026-06-24T00:00:00Z&end=2026-09-22T00:00:00Z",
    )
    .await;
    assert_eq!(at_cap.status_code(), 200, "{}", at_cap.text());

    assert_invalid_parameter(
        admin_get(
            &server,
            "/v1/admin/platform/revenue-density?start=2026-06-23T23:59:59Z&end=2026-09-22T00:00:00Z",
        )
        .await,
    );
}

#[tokio::test]
async fn organization_metrics_rejects_malformed_and_reversed_bounds() {
    let server = setup_hygiene_server().await;
    let organization = create_org(&server).await;

    for query in [
        "start=not-a-timestamp",
        "end=not-a-timestamp",
        "start=2026-09-22T00:00:00Z&end=2026-09-21T00:00:00Z",
        "start=2026-09-22T00:00:00Z&end=2026-09-22T00:00:00Z",
    ] {
        assert_invalid_parameter(
            admin_get(
                &server,
                &format!(
                    "/v1/admin/organizations/{}/metrics?{query}",
                    organization.id
                ),
            )
            .await,
        );
    }
}

#[tokio::test]
async fn organization_metrics_uses_30_day_default_and_366_day_maximum() {
    let server = setup_hygiene_server().await;
    let organization = create_org(&server).await;

    let default = admin_get(
        &server,
        &format!("/v1/admin/organizations/{}/metrics", organization.id),
    )
    .await;
    assert_eq!(default.status_code(), 200, "{}", default.text());
    let metrics = default.json::<OrganizationMetrics>();
    assert_eq!(
        metrics.period_end - metrics.period_start,
        chrono::Duration::days(30)
    );

    let at_max = admin_get(
        &server,
        &format!(
            "/v1/admin/organizations/{}/metrics?start=2025-09-21T00:00:00Z&end=2026-09-22T00:00:00Z",
            organization.id
        ),
    )
    .await;
    assert_eq!(at_max.status_code(), 200, "{}", at_max.text());

    assert_invalid_parameter(
        admin_get(
            &server,
            &format!(
                "/v1/admin/organizations/{}/metrics?start=2025-09-20T23:59:59Z&end=2026-09-22T00:00:00Z",
                organization.id
            ),
        )
        .await,
    );
}
