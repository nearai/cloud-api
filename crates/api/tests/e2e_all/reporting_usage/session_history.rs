use crate::common::{
    create_api_key_in_workspace, create_org, list_workspaces, setup_qwen_model,
};
use chrono::{DateTime, TimeZone};
use uuid::Uuid;

#[tokio::test]
async fn session_usage_history_filters() {
    // Given: one org has usage on two dates and another row on the filtered date in a different workspace/API key.
    let (server, database) = setup_reporting_usage_server().await;
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
    assert_eq!(body.total, 1, "filtered total should count only matching rows");
    assert_eq!(body.limit, 10);
    assert_eq!(body.offset, 0);
    assert_eq!(body.data.len(), 1);
    let row = &body.data[0];
    assert_eq!(row.workspace_id, fixture.workspace_id);
    assert_eq!(row.api_key_id, fixture.api_key_id);
    assert_eq!(row.created_at, ts(2026, 7, 2).to_rfc3339());
    assert_eq!(row.total_cost, 700);
    assert_eq!(row.stop_reason.as_deref(), Some("completed"));
    assert_eq!(row.provider_request_id, None);

    let manual = serde_json::json!({
        "status": 200,
        "total": body.total,
        "limit": body.limit,
        "offset": body.offset,
        "data": body.data,
    });
    assert_no_private_session_history_fields(&manual);
    println!("manual GET /usage/history filtered session 200 {manual}");
}

#[tokio::test]
async fn session_usage_history_rejects_invalid_filter_range() {
    // Given: an organization with session-authenticated usage history access.
    let (server, database) = setup_reporting_usage_server().await;
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
    println!("manual GET /usage/history invalid session range 400 {body}");
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
        ts(2026, 7, 1),
        500,
    )
    .await;
    insert_inference_usage_row(
        database,
        &org.id,
        &workspace_id,
        &api_key.id,
        &model,
        ts(2026, 7, 2),
        700,
    )
    .await;
    insert_inference_usage_row(
        database,
        &org.id,
        &other_workspace,
        &other_api_key.id,
        &model,
        ts(2026, 7, 2),
        900,
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

async fn insert_inference_usage_row(
    database: &Arc<Database>,
    org_id: &str,
    workspace_id: &str,
    api_key_id: &str,
    model: &str,
    created_at: DateTime<Utc>,
    total_cost: i64,
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
                created_at, inference_id, stop_reason
            )
            VALUES ($1, $2, $3, $4, $5, $6, 4, 3, 1, 8, $7, 300, $8,
                NULL, 'chat_completion', $9, $10, 'completed')
            "#,
            &[
                &Uuid::new_v4(),
                &Uuid::parse_str(org_id).expect("org uuid"),
                &Uuid::parse_str(workspace_id).expect("workspace uuid"),
                &Uuid::parse_str(api_key_id).expect("api key uuid"),
                &model_id,
                &model,
                &(total_cost - 300),
                &total_cost,
                &created_at,
                &Some(Uuid::new_v4()),
            ],
        )
        .await
        .expect("insert inference usage");
}

fn assert_no_private_session_history_fields(value: &Value) {
    let text = value.to_string();
    for private in ["prompt", "response_body", "file", "token_hash", "Authorization", "Bearer"] {
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
