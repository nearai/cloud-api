use super::create_reporting_token;
use crate::common::{
    create_api_key_in_workspace, create_org, get_or_create_web_search_service, list_workspaces,
    setup_qwen_model,
};
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use database::Database;
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

pub struct ExportFixture {
    pub org_id: String,
    pub token: String,
    pub workspace_id: String,
    pub api_key_id: String,
    pub model: String,
    pub service_name: String,
}

pub async fn seed_export_fixture(
    server: &axum_test::TestServer,
    database: &Arc<Database>,
) -> ExportFixture {
    let org = create_org(server).await;
    let workspaces = list_workspaces(server, org.id.clone()).await;
    let workspace_id = workspaces
        .first()
        .expect("workspace should exist")
        .id
        .clone();
    let api_key = create_api_key_in_workspace(
        server,
        workspace_id.clone(),
        "reporting export key".to_string(),
    )
    .await;
    let model = setup_qwen_model(server).await;
    let service = get_or_create_web_search_service(server).await;
    let token = create_reporting_token(server, &org.id).await;

    insert_inference_usage(database, &org.id, &workspace_id, &api_key.id, &model).await;
    insert_service_usage(database, &org.id, &workspace_id, &api_key.id, service.id).await;

    ExportFixture {
        org_id: org.id,
        token,
        workspace_id,
        api_key_id: api_key.id,
        model,
        service_name: service.service_name,
    }
}

pub fn assert_no_private_export_fields(value: &Value) {
    let text = value.to_string();
    for private in [
        "provider_request_id",
        "prompt",
        "response_body",
        "file",
        "token_hash",
        "Authorization",
        "Bearer",
        "rpt-",
    ] {
        assert!(
            !text.contains(private),
            "private field leaked: {private}: {text}"
        );
    }
}

pub fn redacted_export(value: &Value) -> Value {
    let mut redacted = value.clone();
    if let Some(cursor) = redacted.get_mut("next_cursor") {
        *cursor = Value::String("<redacted-cursor>".to_string());
    }
    redacted
}

pub fn ts(year: i32, month: u32, day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, 0, 0, 0)
        .single()
        .expect("fixture timestamp")
}

pub fn url_ts(year: i32, month: u32, day: u32) -> String {
    ts(year, month, day).to_rfc3339_opts(SecondsFormat::Secs, true)
}

async fn insert_inference_usage(
    database: &Arc<Database>,
    org_id: &str,
    workspace_id: &str,
    api_key_id: &str,
    model: &str,
) {
    let client = database.pool().get().await.expect("db connection");
    let model_id: Uuid = client
        .query_one("SELECT id FROM models WHERE model_name = $1", &[&model])
        .await
        .expect("model id")
        .get("id");
    for (created_at, input_tokens, total_cost) in
        [(ts(2026, 7, 2), 7, 700_i64), (ts(2026, 7, 1), 5, 500_i64)]
    {
        client
            .execute(
                r#"
                INSERT INTO organization_usage_log (
                    id, organization_id, workspace_id, api_key_id, model_id, model_name,
                    input_tokens, output_tokens, cache_read_tokens, total_tokens,
                    input_cost, output_cost, total_cost, request_type, inference_type,
                    created_at, inference_id, provider_request_id, stop_reason
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, 3, 1, $8, $9, 300, $10,
                    NULL, 'chat_completion', $11, $12, $13, 'completed')
                "#,
                &[
                    &Uuid::new_v4(),
                    &Uuid::parse_str(org_id).expect("org uuid"),
                    &Uuid::parse_str(workspace_id).expect("workspace uuid"),
                    &Uuid::parse_str(api_key_id).expect("api key uuid"),
                    &model_id,
                    &model,
                    &input_tokens,
                    &(input_tokens + 3),
                    &(total_cost - 300),
                    &total_cost,
                    &created_at,
                    &Some(Uuid::new_v4()),
                    &Some(format!("private-provider-request-{total_cost}")),
                ],
            )
            .await
            .expect("insert inference usage");
    }
}

async fn insert_service_usage(
    database: &Arc<Database>,
    org_id: &str,
    workspace_id: &str,
    api_key_id: &str,
    service_id: Uuid,
) {
    let client = database.pool().get().await.expect("db connection");
    for (created_at, quantity, total_cost) in
        [(ts(2026, 7, 3), 1, 100_i64), (ts(2026, 7, 2), 2, 200_i64)]
    {
        client
            .execute(
                r#"
                INSERT INTO organization_service_usage_log (
                    id, organization_id, workspace_id, api_key_id, service_id,
                    quantity, total_cost, inference_id, created_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, NULL, $8)
                "#,
                &[
                    &Uuid::new_v4(),
                    &Uuid::parse_str(org_id).expect("org uuid"),
                    &Uuid::parse_str(workspace_id).expect("workspace uuid"),
                    &Uuid::parse_str(api_key_id).expect("api key uuid"),
                    &service_id,
                    &quantity,
                    &total_cost,
                    &created_at,
                ],
            )
            .await
            .expect("insert service usage");
    }
}
