//! The app router with the cloud-platform adapters pointed at a mock
//! cloud-platform: `/v1/check_api_key` is cloud-platform's verdict, and an
//! unlisted model's grant is read from cloud-platform with the caller's key.

use crate::common::*;
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Well formed (`sk-` and 32 more characters) but unknown to the local database.
const CP_VALID: &str = "sk-c0ffee00c0ffee00c0ffee00c0ffee00";
const CP_UNKNOWN: &str = "sk-dead0000dead0000dead0000dead0000";
const VERDICT: &str = r#"{"valid":true,"organization_id":"o","workspace_id":"w","api_key_id":"k"}"#;

fn cp_client(server: &MockServer) -> Arc<database::cp_client::CpClient> {
    Arc::new(
        database::cp_client::CpClient::new(
            &server.uri(),
            Some("sk-service".to_string()),
            &database::cp_client::CpClientConfig::default(),
            None,
        )
        .unwrap(),
    )
}

#[tokio::test]
async fn check_api_key_is_cloud_platforms_verdict_and_grants_use_the_callers_key() {
    let cp = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/check_api_key"))
        .and(header(
            "authorization",
            format!("Bearer {CP_VALID}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(VERDICT))
        .mount(&cp)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/check_api_key"))
        .and(header(
            "authorization",
            format!("Bearer {CP_UNKNOWN}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"valid":false}"#))
        .mount(&cp)
        .await;

    let client = cp_client(&cp);
    let (server, _db) = setup_test_server_with_domain_services(|domain, db| {
        domain.cloud_platform_check_api_key = Some(Arc::new(
            database::cp_client::ApiKeyVerdicts::new(client.clone(), Duration::from_secs(60)),
        ));
        let local = Arc::new(database::repositories::ModelRepository::new(
            db.pool().clone(),
        ));
        domain.models_service = Arc::new(services::models::ModelsServiceImpl::new(
            domain.inference_provider_pool.clone(),
            Arc::new(database::cp_client::CatalogModels::new(
                local,
                client.clone(),
                Duration::from_secs(30),
                Duration::from_secs(300),
                None,
            )),
        ));
    })
    .await;
    // Request scopes copy the caller's key only when the client is configured.
    database::cp_client::client::set_configured(true);

    // /v1/check_api_key: cloud-platform's verdict, body byte for byte.
    let response = server
        .post("/v1/check_api_key")
        .add_header("Authorization", format!("Bearer {CP_VALID}"))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(response.text(), VERDICT);
    let response = server
        .post("/v1/check_api_key")
        .add_header("Authorization", format!("Bearer {CP_UNKNOWN}"))
        .await;
    assert_eq!(response.status_code(), 401, "{}", response.text());

    // An unlisted model: its grant is asked for with the caller's own key.
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let model_name = format!("cp-e2e/unlisted-{}", uuid::Uuid::new_v4());
    Mock::given(method("GET"))
        .and(path("/v1/internal/catalog/model-lookup"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": uuid::Uuid::new_v4(),
            "model_name": model_name,
            "model_display_name": model_name,
            "model_description": "d",
            "input_cost_per_token": 1,
            "output_cost_per_token": 2,
            "cost_per_image": 0,
            "context_length": 4096,
            "verifiable": false,
            "owned_by": "x",
            "provider_type": "vllm",
            "attestation_supported": false,
            "is_listed": false,
            "created_at": "2026-01-01T00:00:00Z",
        })))
        .mount(&cp)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/internal/catalog/model-access"))
        .and(header(
            "authorization",
            format!("Bearer {api_key}").as_str(),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"has_access": true})),
        )
        .expect(1..)
        .named("model access with the caller's key")
        .mount(&cp)
        .await;
    // The model has no backend here, so the request itself cannot succeed;
    // what is checked is the grant read above, verified when `cp` drops.
    let response = server
        .post("/v1/score")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"model": model_name, "text_1": "a", "text_2": "b"}))
        .await;
    assert_ne!(response.status_code(), 401, "{}", response.text());
    cp.verify().await;
}
