//! E2E boundary coverage for the stateless Responses API.

use crate::common::*;
use axum::http::Method;

fn assert_response_history_is_gone(response: axum_test::TestResponse) {
    assert_eq!(response.status_code(), 410);
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );

    let error = response.json::<api::models::ErrorResponse>();
    assert_eq!(error.error.r#type, "gone");
    assert!(error.error.message.contains("stateless"));
}

#[tokio::test]
async fn retired_response_history_routes_require_authentication_then_return_gone() {
    let server = setup_test_server().await;

    assert_eq!(
        server.get("/v1/responses/resp_example").await.status_code(),
        401
    );

    let (api_key, _) = create_org_and_api_key(&server).await;
    let routes = [
        (Method::GET, "/v1/responses/resp_example"),
        (Method::DELETE, "/v1/responses/resp_example"),
        (Method::POST, "/v1/responses/resp_example/cancel"),
        (Method::GET, "/v1/responses/resp_example/input_items"),
    ];

    for (method, path) in routes {
        assert_response_history_is_gone(
            server
                .method(method.clone(), path)
                .add_header("Authorization", format!("Bearer {api_key}"))
                .await,
        );
    }
}

#[tokio::test]
async fn stateless_responses_reject_persistent_fields() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let requests = [
        serde_json::json!({"model": "test-model", "input": "hello", "store": true}),
        serde_json::json!({
            "model": "test-model",
            "input": "hello",
            "store": false,
            "conversation": "conv_example"
        }),
        serde_json::json!({
            "model": "test-model",
            "input": "hello",
            "store": false,
            "previous_response_id": "resp_example"
        }),
        serde_json::json!({
            "model": "test-model",
            "input": "hello",
            "store": false,
            "background": true
        }),
        serde_json::json!({
            "model": "test-model",
            "store": false,
            "input": [{
                "role": "user",
                "content": [{"type": "input_file", "file_id": "file_example"}]
            }]
        }),
    ];

    for request in requests {
        let response = server
            .post("/v1/responses")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&request)
            .await;

        assert_eq!(
            response.status_code(),
            400,
            "request {request} must be rejected"
        );
        let error = response.json::<api::models::ErrorResponse>();
        assert_eq!(error.error.r#type, "invalid_request_error");
    }
}

#[tokio::test]
async fn completed_stateless_response_exposes_gateway_signature_when_persisted() {
    let server = setup_test_server().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let request = serde_json::json!({
        "model": model,
        "input": "Respond with a short attested answer.",
        "stream": false,
        "store": false
    });
    let expected_request_hash = compute_sha256(
        &serde_json::to_string(&request).expect("serialize stateless Responses request"),
    );

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&request)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "stateless Responses request should succeed: {}",
        response.text()
    );
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );

    let response_text = response.text();
    let expected_response_hash = compute_sha256(&response_text);
    let response_json: serde_json::Value =
        serde_json::from_str(&response_text).expect("Responses result must be JSON");
    let response_id = response_json
        .get("id")
        .and_then(|value| value.as_str())
        .expect("completed response must have an ID");
    assert!(response_id.starts_with("resp_"));

    let signature = server
        .get(&format!("/v1/signature/{response_id}?signing_algo=ecdsa"))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(
        signature.status_code(),
        200,
        "completed response signature should be available when persistence succeeds: {}",
        signature.text()
    );

    let signature: serde_json::Value = signature.json();
    assert_eq!(
        signature
            .get("signature_kind")
            .and_then(|value| value.as_str()),
        Some("gateway")
    );
    assert_eq!(
        signature
            .get("signing_algo")
            .and_then(|value| value.as_str()),
        Some("ecdsa")
    );
    assert!(
        signature
            .get("signature")
            .and_then(|value| value.as_str())
            .is_some_and(|value| !value.is_empty()),
        "gateway signature must be present"
    );

    let signed_hashes = signature
        .get("text")
        .and_then(|value| value.as_str())
        .expect("gateway signature must include its signed hashes");
    let (request_hash, response_hash) = signed_hashes
        .split_once(':')
        .expect("gateway signature text must be request_hash:response_hash");
    assert_eq!(request_hash, expected_request_hash);
    assert_eq!(response_hash, expected_response_hash);
}
