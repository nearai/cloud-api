//! E2E coverage for retired client-executed function tooling.
//!
//! Stateless Responses requests cannot pause for a client to execute a
//! function and submit a continuation. Keep the boundary assertions here
//! instead of the former multi-turn lifecycle tests.

use crate::common::*;

#[tokio::test]
async fn function_tools_are_rejected_by_stateless_responses() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "test-model",
            "input": "What is the weather?",
            "store": false,
            "tools": [{
                "type": "function",
                "name": "get_weather",
                "parameters": {"type": "object"}
            }]
        }))
        .await;

    assert_eq!(response.status_code(), 400);
    let error = response.json::<api::models::ErrorResponse>();
    assert_eq!(error.error.r#type, "invalid_request_error");
    assert!(error.error.message.contains("function tools"));
}

#[tokio::test]
async fn function_continuations_are_rejected_by_stateless_responses() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "test-model",
            "store": false,
            "input": [{
                "type": "function_call_output",
                "call_id": "call_example",
                "output": "{\"temperature\":22}"
            }]
        }))
        .await;

    assert_eq!(response.status_code(), 400);
    let error = response.json::<api::models::ErrorResponse>();
    assert_eq!(error.error.r#type, "invalid_request_error");
    assert!(error.error.message.contains("function continuation"));
}
