//! E2E tests: request DESERIALIZATION-layer rejections must return the same
//! OpenAI error envelope as business-validation errors, not a bare string.
//!
//! Regression coverage for issue #781 (L2). Before the `OpenAiJson` extractor,
//! axum's built-in `Json<T>` rejected malformed/invalid bodies with a bare
//! `text/plain` string and a `422` for shape/type errors. Now these surface as
//! `{ "error": { "message", "type": "invalid_request_error", "param", "code" } }`
//! with a `400` status, matching what clients get from business validation.
//!
//! The rejection fires AFTER auth/usage/rate-limit middleware, so each test
//! needs a real org + credits + API key to reach the extractor.

use crate::common::*;
use bytes::Bytes;
use serde_json::json;

/// Helper: set up server + funded org + API key, returning (server, api_key).
async fn server_with_key() -> (axum_test::TestServer, String) {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    (server, api_key)
}

/// Assert a response is the OpenAI error envelope with the expected status and
/// `type: "invalid_request_error"`, and that the message is non-empty.
fn assert_openai_invalid_request(response: &axum_test::TestResponse, expected_status: u16) {
    assert_eq!(
        response.status_code(),
        expected_status,
        "expected {} for deser-layer rejection, got body: {}",
        expected_status,
        response.text()
    );

    // Must deserialize into the OpenAI envelope — a bare string body would fail here.
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(
        err.error.r#type, "invalid_request_error",
        "deser-layer error type should match business-validation envelope"
    );
    assert!(
        !err.error.message.is_empty(),
        "error message should be populated, got empty string"
    );
}

#[tokio::test]
async fn test_chat_completions_malformed_json_returns_openai_envelope() {
    let (server, api_key) = server_with_key().await;

    // Syntactically invalid JSON (trailing junk / unterminated object).
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .content_type("application/json")
        .bytes(Bytes::from_static(b"{ \"model\": \"x\", "))
        .await;

    assert_openai_invalid_request(&response, 400);
}

#[tokio::test]
async fn test_chat_completions_missing_model_returns_openai_envelope() {
    let (server, api_key) = server_with_key().await;

    // Valid JSON but missing the required `model` field. axum's default Json
    // would reject this as 422 with a bare string; we normalize to 400 + envelope.
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "messages": [ { "role": "user", "content": "hello" } ]
        }))
        .await;

    assert_openai_invalid_request(&response, 400);
}

#[tokio::test]
async fn test_chat_completions_wrong_type_messages_returns_openai_envelope() {
    let (server, api_key) = server_with_key().await;

    // `messages` is the wrong type (string instead of array). Valid JSON,
    // wrong shape -> 400 + OpenAI envelope (was 422 + bare string).
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": E2E_QWEN_MODEL_NAME,
            "messages": "not-an-array"
        }))
        .await;

    assert_openai_invalid_request(&response, 400);
}

#[tokio::test]
async fn test_completions_malformed_json_returns_openai_envelope() {
    let (server, api_key) = server_with_key().await;

    let response = server
        .post("/v1/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .content_type("application/json")
        .bytes(Bytes::from_static(b"not json at all"))
        .await;

    assert_openai_invalid_request(&response, 400);
}

#[tokio::test]
async fn test_completions_missing_model_returns_openai_envelope() {
    let (server, api_key) = server_with_key().await;

    let response = server
        .post("/v1/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({ "prompt": "hello" }))
        .await;

    assert_openai_invalid_request(&response, 400);
}

/// Sanity check: a VALID request is unaffected by the extractor change — it
/// must NOT be turned into a 400. (Guards against the extractor over-reaching.)
#[tokio::test]
async fn test_chat_completions_valid_request_unaffected() {
    let (server, api_key) = server_with_key().await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": E2E_QWEN_MODEL_NAME,
            "messages": [ { "role": "user", "content": "hello" } ]
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "valid request should still succeed: {}",
        response.text()
    );
}
