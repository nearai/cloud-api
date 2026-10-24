//! E2E coverage for metadata accepted by a stateless Responses request.

use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn request_metadata_is_returned_by_a_stateless_response() {
    let (server, _pool, mock, _database) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock.set_default_response(inference_providers::mock::ResponseTemplate::new(
        "metadata reply",
    ))
    .await;

    let metadata = json!({
        "source": "e2e",
        "request_id": "client-managed-context",
    });
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": model,
            "input": [{
                "role": "user",
                "content": "Hello, world!",
                "metadata": {"source": "client"}
            }],
            "metadata": metadata,
            "store": false,
            "stream": false,
            "max_output_tokens": 10
        }))
        .await;

    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    let response: api::models::ResponseObject = response.json();
    assert!(!response.store);
    assert_eq!(response.metadata, Some(metadata));
}

#[tokio::test]
async fn oversized_input_metadata_is_rejected_without_a_conversation() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let large_string = "x".repeat(17 * 1024);

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": "test-model",
            "input": [{
                "role": "user",
                "content": "Hello",
                "metadata": {"large_field": large_string}
            }],
            "store": false,
            "stream": false
        }))
        .await;

    assert_eq!(response.status_code(), 400);
    let error: api::models::ErrorResponse = response.json();
    assert!(error.error.message.contains("metadata is too large"));
}
