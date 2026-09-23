// E2E tests for GET /v1/attestation/report parameter handling:
// - `signing_algo` is case-insensitive, and the backend receives the lowercase
//   value (the mock backend, like inference-proxy, rejects anything else);
// - an unknown model is reported exactly like /v1/chat/completions does
//   (400 invalid_request_error, param "model"), not as a 503 provider error.

use crate::common::*;

fn report_url(model: &str) -> String {
    let encoded = url::form_urlencoded::byte_serialize(model.as_bytes()).collect::<String>();
    format!("/v1/attestation/report?model={encoded}")
}

#[tokio::test]
async fn test_attestation_report_accepts_mixed_case_signing_algo() {
    let (server, _router, _pool, mock_provider, _database) =
        setup_test_server_with_pool_and_router().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let nonce = "deadbeef".repeat(8);

    for (requested, normalized) in [("ECDSA", "ecdsa"), ("Ed25519", "ed25519")] {
        // With a nonce the request bypasses the report cache; without one it
        // goes through it. Each no-nonce key is requested once, so both paths
        // reach the backend exactly once.
        for nonce_param in [format!("&nonce={nonce}"), String::new()] {
            let url = format!(
                "{}&signing_algo={requested}{nonce_param}",
                report_url(&model)
            );
            let backend_calls_before = mock_provider.attestation_signing_algos().len();

            let response = server
                .get(&url)
                .add_header("Authorization", format!("Bearer {api_key}"))
                .await;

            assert_eq!(
                response.status_code(),
                200,
                "{url} must succeed: {}",
                response.text()
            );
            let body: serde_json::Value = response.json();
            assert_eq!(
                body["gateway_attestation"]["signing_algo"], normalized,
                "{url}: {body}"
            );
            let forwarded =
                mock_provider.attestation_signing_algos()[backend_calls_before..].to_vec();
            assert_eq!(forwarded, vec![Some(normalized.to_string())], "{url}");
        }
    }
}

#[tokio::test]
async fn test_attestation_report_unknown_model_matches_chat_completions() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let unknown = format!("test-unknown/model-{}", uuid::Uuid::new_v4());

    let chat = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": unknown,
            "messages": [{ "role": "user", "content": "Hello" }],
            "max_tokens": 10
        }))
        .await;
    assert_eq!(chat.status_code(), 400, "{}", chat.text());
    let chat_body: serde_json::Value = chat.json();
    assert_eq!(chat_body["error"]["type"], "invalid_request_error");
    assert_eq!(chat_body["error"]["param"], "model");

    let nonce = "deadbeef".repeat(8);
    for query in [format!("&nonce={nonce}"), String::new()] {
        // x-no-aliasing only rejects aliases; an unknown model gets the same
        // error with or without it.
        for no_aliasing in [false, true] {
            let url = format!("{}{query}", report_url(&unknown));
            let mut request = server
                .get(&url)
                .add_header("Authorization", format!("Bearer {api_key}"));
            if no_aliasing {
                request = request.add_header("x-no-aliasing", "true");
            }
            let response = request.await;

            assert_eq!(
                response.status_code(),
                400,
                "{url} (x-no-aliasing={no_aliasing}): {}",
                response.text()
            );
            let body: serde_json::Value = response.json();
            assert_eq!(
                body, chat_body,
                "{url} (x-no-aliasing={no_aliasing}) must match the chat completions error"
            );
        }
    }
}
