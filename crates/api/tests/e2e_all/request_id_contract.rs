use crate::common::{
    get_api_key_for_org, list_workspaces, setup_org_with_credits, setup_qwen_model,
    setup_test_server_with_search_providers, MockWebSearchProvider,
};
use axum::http::Method;
use std::sync::Arc;
use std::sync::OnceLock;
use uuid::Uuid;

const REQUEST_ID_HEADER: &str = "x-request-id";
const ORG_ID_HEADER: &str = "x-org-id";
const WORKSPACE_ID_HEADER: &str = "x-workspace-id";
static DEV_ENV: OnceLock<()> = OnceLock::new();

fn allow_debug_attestation_keys() {
    DEV_ENV.get_or_init(|| {
        std::env::set_var("DEV", "1");
        std::env::set_var("BRAVE_SEARCH_PRO_API_KEY", "request-id-contract-test");
    });
}

async fn setup_request_id_server() -> (
    axum_test::TestServer,
    Arc<inference_providers::mock::MockProvider>,
) {
    allow_debug_attestation_keys();
    let (server, _, mock_provider) = setup_test_server_with_search_providers(
        Arc::new(MockWebSearchProvider::default_results()),
        None,
    )
    .await;
    (server, mock_provider)
}

fn request_id(response: &axum_test::TestResponse) -> Uuid {
    let value = response
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|header| header.to_str().ok())
        .expect("response should include x-request-id");
    Uuid::parse_str(value).expect("x-request-id should be a UUID")
}

fn assert_uuid_response_id(label: &str, response: &axum_test::TestResponse) -> Uuid {
    let id = request_id(response);
    println!(
        "request_id_contract {label}: status={} x-request-id={id}",
        response.status_code()
    );
    id
}

fn header_value(response: &axum_test::TestResponse, name: &str) -> String {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .expect("response should include expected header")
        .to_string()
}

fn assert_header_csv_contains(response: &axum_test::TestResponse, name: &str, expected: &str) {
    let header = header_value(response, name);
    assert!(
        header
            .split(',')
            .map(str::trim)
            .any(|part| part == "*" || part.eq_ignore_ascii_case(expected)),
        "{name} should include {expected}; actual value: {header}"
    );
}

#[tokio::test]
async fn test_request_id_contract_for_public_and_error_surfaces() {
    // Given
    let (server, _) = setup_request_id_server().await;
    let valid_id = Uuid::new_v4();
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let model = setup_qwen_model(&server).await;

    // When
    let health = server
        .get("/v1/health")
        .add_header(REQUEST_ID_HEADER, valid_id.to_string())
        .await;
    let invalid = server
        .get("/v1/health")
        .add_header(REQUEST_ID_HEADER, "not-a-uuid")
        .await;
    let absent = server.get("/v1/health").await;
    let unknown = server.get("/v1/unknown-route-for-request-id").await;
    let auth = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "request-id-contract",
            "messages": [{"role": "user", "content": "sentinel-auth"}]
        }))
        .await;
    let validation = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": []
        }))
        .await;

    // Then
    assert_eq!(
        assert_uuid_response_id("success valid reuse", &health),
        valid_id
    );

    let invalid_id = assert_uuid_response_id("invalid replacement", &invalid);
    assert_ne!(invalid_id.to_string(), "not-a-uuid");

    let absent_id = assert_uuid_response_id("generated absent", &absent);
    assert_ne!(absent_id, invalid_id);

    assert_uuid_response_id("unknown route error", &unknown);
    assert_uuid_response_id("auth error", &auth);
    assert_eq!(validation.status_code(), 400);
    assert_uuid_response_id("validation error", &validation);
}

#[tokio::test]
async fn test_request_id_contract_for_cors_and_streaming_surfaces() {
    // Given
    let (server, mock_provider) = setup_request_id_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace_id = workspaces
        .first()
        .expect("test org should have a default workspace")
        .id
        .clone();
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    let model = setup_qwen_model(&server).await;
    let spoofed_org_id = Uuid::new_v4().to_string();
    let spoofed_workspace_id = Uuid::new_v4().to_string();
    let inbound_request_id = Uuid::new_v4();

    // When
    let cors = server
        .method(Method::OPTIONS, "/v1/health")
        .add_header("Origin", "http://localhost:3000")
        .add_header("Access-Control-Request-Method", "GET")
        .add_header("Access-Control-Request-Headers", REQUEST_ID_HEADER)
        .await;
    let cors_actual = server
        .get("/v1/health")
        .add_header("Origin", "http://localhost:3000")
        .await;
    let streaming = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header(REQUEST_ID_HEADER, inbound_request_id.to_string())
        .add_header(ORG_ID_HEADER, spoofed_org_id.clone())
        .add_header(WORKSPACE_ID_HEADER, spoofed_workspace_id.clone())
        .json(&serde_json::json!({
            "model": model,
            "stream": true,
            "messages": [{"role": "user", "content": "sentinel-stream"}]
        }))
        .await;

    // Then
    assert_uuid_response_id("CORS preflight", &cors);
    assert_header_csv_contains(&cors, "access-control-allow-headers", REQUEST_ID_HEADER);
    println!(
        "request_id_contract CORS allow status={} x-request-id_allowed=true access-control-allow-headers={}",
        cors.status_code(),
        header_value(&cors, "access-control-allow-headers")
    );
    assert_uuid_response_id("CORS actual", &cors_actual);
    assert_header_csv_contains(
        &cors_actual,
        "access-control-expose-headers",
        REQUEST_ID_HEADER,
    );
    println!(
        "request_id_contract CORS expose status={} x-request-id_exposed=true access-control-expose-headers={}",
        cors_actual.status_code(),
        header_value(&cors_actual, "access-control-expose-headers")
    );
    let selected_id = assert_uuid_response_id("streaming success", &streaming);
    assert_eq!(streaming.status_code(), 200);
    assert_eq!(selected_id, inbound_request_id);
    println!(
        "request_id_contract streaming headers observed before body chunks status={} x-request-id={selected_id}",
        streaming.status_code()
    );
    let streaming_body = streaming.text();
    assert!(
        streaming_body.contains("[DONE]"),
        "streaming test must drain the finite SSE response"
    );
    let params = mock_provider
        .last_chat_params()
        .await
        .expect("streaming request should reach mock provider");
    assert_eq!(
        params
            .extra
            .get("x_request_id")
            .and_then(serde_json::Value::as_str),
        Some(selected_id.to_string().as_str()),
        "provider propagation should use selected middleware request ID"
    );
    assert_eq!(
        params
            .extra
            .get("x_org_id")
            .and_then(serde_json::Value::as_str),
        Some(org.id.as_str()),
        "provider propagation should use the authenticated organization"
    );
    assert_eq!(
        params
            .extra
            .get("x_workspace_id")
            .and_then(serde_json::Value::as_str),
        Some(workspace_id.as_str()),
        "provider propagation should use the authenticated workspace"
    );
    assert_ne!(
        params
            .extra
            .get("x_org_id")
            .and_then(serde_json::Value::as_str),
        Some(spoofed_org_id.as_str()),
        "public tenant spoofing headers must not propagate"
    );
    assert_ne!(
        params
            .extra
            .get("x_workspace_id")
            .and_then(serde_json::Value::as_str),
        Some(spoofed_workspace_id.as_str()),
        "public workspace spoofing headers must not propagate"
    );
    println!("request_id_contract tenant spoof rejection: public tenant headers ignored");
}
