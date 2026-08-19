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
