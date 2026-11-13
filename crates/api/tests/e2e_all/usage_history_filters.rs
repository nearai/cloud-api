//! End-to-end coverage of the `GET /v1/organizations/{id}/usage/history` filter
//! and range handling.

use crate::common::{
    create_api_key_in_workspace, create_org, get_session_id, list_workspaces, setup_qwen_model,
    setup_test_server_with_database, MOCK_USER_AGENT,
};
use chrono::{DateTime, Duration, TimeZone, Utc};
use database::Database;
use serde_json::Value;
use std::sync::Arc;
use utoipa::OpenApi;
use uuid::Uuid;

#[tokio::test]
async fn session_usage_history_filters() {
    // Given: one org has usage on two dates and another row on the filtered date in a different workspace/API key.
    let (server, database) = setup_test_server_with_database().await;
    let fixture = seed_session_history_fixture(&server, &database).await;

    // When: the session-authenticated history endpoint receives dashboard-style filters.
    let response = server
        .get(
            format!(
                "/v1/organizations/{}/usage/history?start_date=2026-07-02&end_date=2026-07-02&workspace_id={}&api_key_id={}&limit=10&offset=0",
                fixture.org_id, fixture.workspace_id, fixture.api_key_id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    // Then: only the row matching the date, workspace, and API key filters is returned.
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body = response.json::<api::routes::usage::UsageHistoryResponse>();
    assert_eq!(
        body.total, 1,
        "filtered total should count only matching rows"
    );
    assert_eq!(body.limit, 10);
    assert_eq!(body.offset, 0);
    assert_eq!(body.data.len(), 1);
    let row = &body.data[0];
    assert_eq!(row.workspace_id, fixture.workspace_id);
    assert_eq!(row.api_key_id, fixture.api_key_id);
    assert_eq!(row.created_at, ts(2026, 7, 2).to_rfc3339());
    assert_eq!(row.total_cost, 700);
    assert_eq!(row.stop_reason.as_deref(), Some("completed"));
    assert_eq!(
        row.provider_request_id.as_deref(),
        Some("provider-request-700")
    );

    let manual = serde_json::json!({
        "status": 200,
        "total": body.total,
        "limit": body.limit,
        "offset": body.offset,
        "data": body.data,
    });
    assert_no_private_session_history_fields(&manual);
}

#[tokio::test]
async fn session_usage_history_workspace_and_api_key_filters_remain_unbounded() {
    // Given: matching usage older than the reporting API's 366-day default window.
    let (server, database) = setup_test_server_with_database().await;
    let fixture = seed_session_history_fixture(&server, &database).await;

    // When: only workspace and API-key filters are supplied to the legacy history route.
    let response = server
        .get(
            format!(
                "/v1/organizations/{}/usage/history?workspace_id={}&api_key_id={}&limit=10&offset=0",
                fixture.org_id, fixture.workspace_id, fixture.api_key_id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    // Then: all matching history is returned, including the old row.
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body = response.json::<api::routes::usage::UsageHistoryResponse>();
    assert_eq!(body.total, 3);
    assert_eq!(body.data.len(), 3);
    assert!(body
        .data
        .iter()
        .any(|row| row.created_at == ts(2024, 1, 1).to_rfc3339()));
    assert!(body
        .data
        .iter()
        .all(|row| row.provider_request_id.is_some()));
}

#[test]
fn session_usage_history_openapi_lists_supported_filters() {
    // Given: the generated public OpenAPI document.
    let spec = serde_json::to_value(api::openapi::ApiDoc::openapi())
        .expect("OpenAPI spec should serialize");

    // When: the legacy organization history operation is inspected.
    let parameters = spec["paths"]["/v1/organizations/{org_id}/usage/history"]["get"]["parameters"]
        .as_array()
        .expect("history parameters should be an array");
    let names: Vec<&str> = parameters
        .iter()
        .filter_map(|parameter| parameter["name"].as_str())
        .collect();

    // Then: every accepted date/time and ownership filter is documented.
    for name in [
        "start_date",
        "end_date",
        "start_time",
        "end_time",
        "workspace_id",
        "api_key_id",
    ] {
        assert!(names.contains(&name), "missing OpenAPI parameter {name}");
    }
}

#[tokio::test]
async fn session_usage_history_rejects_invalid_filter_range() {
    // Given: an organization with session-authenticated usage history access.
    let (server, database) = setup_test_server_with_database().await;
    let fixture = seed_session_history_fixture(&server, &database).await;

    // When: the dashboard-style date range is inverted.
    let response = server
        .get(
            format!(
                "/v1/organizations/{}/usage/history?start_date=2026-07-03&end_date=2026-07-01&limit=10&offset=0",
                fixture.org_id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    // Then: the session route rejects the malformed filter with the existing error envelope.
    assert_eq!(response.status_code(), 400, "{}", response.text());
    let body = response.json::<Value>();
    assert_no_private_session_history_fields(&body);
    assert_eq!(body["error"]["type"], "invalid_reporting_usage_query");
}

#[tokio::test]
async fn session_usage_history_rejects_range_over_366_days() {
    // Given: an organization with usage history access.
    let (server, database) = setup_test_server_with_database().await;
    let fixture = seed_session_history_fixture(&server, &database).await;

    // When: the requested range spans more than 366 days.
    let response = server
        .get(
            format!(
                "/v1/organizations/{}/usage/history?start_date=2024-01-01&end_date=2026-07-01",
                fixture.org_id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    // Then: it is rejected with the same error code as an inverted range.
    assert_eq!(response.status_code(), 400, "{}", response.text());
    let body = response.json::<Value>();
    assert_eq!(body["error"]["type"], "invalid_reporting_usage_query");
}

#[tokio::test]
async fn session_usage_history_omitted_dates_default_to_last_366_days() {
    // Given: one row older than the default window and one recent row.
    let (server, database) = setup_test_server_with_database().await;
    let fixture = seed_session_history_fixture(&server, &database).await;
    let model = setup_qwen_model(&server).await;
    let recent = Utc::now() - Duration::days(1);
    insert_inference_usage_row(
        &database,
        &fixture.org_id,
        &fixture.workspace_id,
        &fixture.api_key_id,
        &model,
        SessionUsageRow {
            created_at: recent,
            total_cost: 1100,
            provider_request_id: "provider-request-recent",
        },
    )
    .await;
    insert_inference_usage_row(
        &database,
        &fixture.org_id,
        &fixture.workspace_id,
        &fixture.api_key_id,
        &model,
        SessionUsageRow {
            created_at: Utc::now() - Duration::days(400),
            total_cost: 1200,
            provider_request_id: "provider-request-stale",
        },
    )
    .await;

    // When: only an end_date filter is supplied, so the start defaults to 366 days earlier.
    let end_date = (Utc::now() + Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    let response = server
        .get(
            format!(
                "/v1/organizations/{}/usage/history?end_date={end_date}&limit=50",
                fixture.org_id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    // Then: the recent row is included and rows outside the window are excluded.
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body = response.json::<api::routes::usage::UsageHistoryResponse>();
    let ids: Vec<_> = body
        .data
        .iter()
        .filter_map(|row| row.provider_request_id.as_deref())
        .collect();
    assert!(ids.contains(&"provider-request-recent"), "{ids:?}");
    assert!(!ids.contains(&"provider-request-stale"), "{ids:?}");
    assert!(!ids.contains(&"provider-request-300"), "{ids:?}");
}

#[tokio::test]
async fn reporting_token_shaped_bearer_is_rejected_with_401() {
    // Given: a bearer credential with the retired reporting-token prefix.
    let (server, _database) = setup_test_server_with_database().await;
    let org = create_org(&server).await;

    // When: it is used on API-key authenticated routes.
    for path in ["/v1/files?limit=1"] {
        let response = server
            .get(path)
            .add_header("Authorization", "Bearer rpt-0123456789abcdef")
            .await;

        // Then: the request is cleanly unauthorized, not a server error.
        assert_eq!(response.status_code(), 401, "{path}: {}", response.text());
    }

    // And: the refresh-token route rejects it as well. The auth double only
    // accepts `rt_` refresh tokens, so this exercises a real rejection.
    let response = server
        .post("/v1/users/me/access-tokens")
        .add_header("Authorization", "Bearer rpt-0123456789abcdef")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 401, "{}", response.text());

    // And: a session-authenticated route never turns it into a server error
    // (the e2e session auth double accepts any bearer, so only the status
    // class is asserted there).
    let response = server
        .get(format!("/v1/organizations/{}/usage/balance", org.id).as_str())
        .add_header("Authorization", "Bearer rpt-0123456789abcdef")
        .await;
    assert!(response.status_code().as_u16() < 500, "{}", response.text());
}

struct SessionHistoryFixture {
    org_id: String,
    workspace_id: String,
    api_key_id: String,
}

async fn seed_session_history_fixture(
    server: &axum_test::TestServer,
    database: &Arc<Database>,
) -> SessionHistoryFixture {
    let org = create_org(server).await;
    let model = setup_qwen_model(server).await;
    let workspaces = list_workspaces(server, org.id.clone()).await;
    let workspace_id = workspaces
        .first()
        .expect("default workspace should exist")
        .id
        .clone();
    let api_key = create_api_key_in_workspace(
        server,
        workspace_id.clone(),
        "session history filtered key".to_string(),
    )
    .await;

    let other_workspace = create_workspace(server, &org.id).await;
    let other_api_key = create_api_key_in_workspace(
        server,
        other_workspace.clone(),
        "session history excluded key".to_string(),
    )
    .await;

    insert_inference_usage_row(
        database,
        &org.id,
        &workspace_id,
        &api_key.id,
        &model,
        SessionUsageRow {
            created_at: ts(2026, 7, 1),
            total_cost: 500,
            provider_request_id: "provider-request-500",
        },
    )
    .await;
    insert_inference_usage_row(
        database,
        &org.id,
        &workspace_id,
        &api_key.id,
        &model,
        SessionUsageRow {
            created_at: ts(2026, 7, 2),
            total_cost: 700,
            provider_request_id: "provider-request-700",
        },
    )
    .await;
    insert_inference_usage_row(
        database,
        &org.id,
        &other_workspace,
        &other_api_key.id,
        &model,
        SessionUsageRow {
            created_at: ts(2026, 7, 2),
            total_cost: 900,
            provider_request_id: "provider-request-900",
        },
    )
    .await;
    insert_inference_usage_row(
        database,
        &org.id,
        &workspace_id,
        &api_key.id,
        &model,
        SessionUsageRow {
            created_at: ts(2024, 1, 1),
            total_cost: 300,
            provider_request_id: "provider-request-300",
        },
    )
    .await;

    SessionHistoryFixture {
        org_id: org.id,
        workspace_id,
        api_key_id: api_key.id,
    }
}

async fn create_workspace(server: &axum_test::TestServer, org_id: &str) -> String {
    let request = api::routes::workspaces::CreateWorkspaceRequest {
        name: format!("session-history-{}", Uuid::new_v4()),
        description: Some("session history filter fixture".to_string()),
    };
    let response = server
        .post(format!("/v1/organizations/{org_id}/workspaces").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&request)
        .await;
    assert_eq!(response.status_code(), 201, "{}", response.text());
    response
        .json::<api::routes::workspaces::WorkspaceResponse>()
        .id
}

struct SessionUsageRow {
    created_at: DateTime<Utc>,
    total_cost: i64,
    provider_request_id: &'static str,
}

async fn insert_inference_usage_row(
    database: &Arc<Database>,
    org_id: &str,
    workspace_id: &str,
    api_key_id: &str,
    model: &str,
    row: SessionUsageRow,
) {
    let client = database.pool().get().await.expect("db connection");
    let model_id: Uuid = client
        .query_one("SELECT id FROM models WHERE model_name = $1", &[&model])
        .await
        .expect("model id")
        .get("id");

    client
        .execute(
            r#"
            INSERT INTO organization_usage_log (
                id, organization_id, workspace_id, api_key_id, model_id, model_name,
                input_tokens, output_tokens, cache_read_tokens, total_tokens,
                input_cost, output_cost, total_cost, request_type, inference_type,
                created_at, inference_id, provider_request_id, stop_reason
            )
            VALUES ($1, $2, $3, $4, $5, $6, 4, 3, 1, 8, $7, 300, $8,
                NULL, 'chat_completion', $9, $10, $11, 'completed')
            "#,
            &[
                &Uuid::new_v4(),
                &Uuid::parse_str(org_id).expect("org uuid"),
                &Uuid::parse_str(workspace_id).expect("workspace uuid"),
                &Uuid::parse_str(api_key_id).expect("api key uuid"),
                &model_id,
                &model,
                &(row.total_cost - 300),
                &row.total_cost,
                &row.created_at,
                &Some(Uuid::new_v4()),
                &Some(row.provider_request_id),
            ],
        )
        .await
        .expect("insert inference usage");
}

fn assert_no_private_session_history_fields(value: &Value) {
    let text = value.to_string();
    for private in [
        "prompt",
        "response_body",
        "file",
        "token_hash",
        "Authorization",
        "Bearer",
    ] {
        assert!(
            !text.contains(private),
            "private field leaked: {private}: {text}"
        );
    }
}

fn ts(year: i32, month: u32, day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, 0, 0, 0)
        .single()
        .expect("fixture timestamp")
}
