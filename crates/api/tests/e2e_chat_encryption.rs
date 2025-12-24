// Import common test utilities
mod common;

use axum_test::TestServer;
use common::*;

use inference_providers::StreamChunk;

// ============================================
// End-to-End Encryption Tests
// ============================================
//
// These tests verify that encryption headers (X-Signing-Algo, X-Client-Pub-Key, and X-Model-Pub-Key)
// are properly extracted and passed through from cloud-api to vllm-proxy.
//
// These tests use the deepseek-ai/DeepSeek-V3.1 model which has encryption support enabled.
// The tests run in CI and require a running vllm-proxy instance with encryption support.

/// Helper function to fetch the model public key from the attestation report endpoint
async fn get_model_public_key(
    server: &TestServer,
    model: &str,
    signing_algo: Option<&str>,
) -> Option<String> {
    let encoded_model = url::form_urlencoded::byte_serialize(model.as_bytes()).collect::<String>();
    let mut url = format!("/v1/attestation/report?model={}", encoded_model);
    if let Some(algo) = signing_algo {
        let encoded_algo =
            url::form_urlencoded::byte_serialize(algo.as_bytes()).collect::<String>();
        url.push_str(&format!("&signing_algo={}", encoded_algo));
    }

    let response = server.get(&url).await;

    if response.status_code() != 200 {
        return None;
    }

    let response_json: serde_json::Value = response.json();

    // Try to get signing_public_key from model_attestations
    if let Some(model_attestations) = response_json
        .get("model_attestations")
        .and_then(|v| v.as_array())
    {
        for attestation in model_attestations {
            if let Some(signing_public_key) = attestation.get("signing_public_key") {
                if let Some(key_str) = signing_public_key.as_str() {
                    return Some(key_str.to_string());
                }
            }
        }
    }

    None
}

/// Test that chat completions with ECDSA encryption headers are passed through correctly
#[tokio::test]
async fn test_chat_completions_with_ecdsa_encryption_headers() {
    let (server, _guard) = setup_test_server().await;
    setup_deepseek_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Fetch model public key from attestation report
    let model = "deepseek-ai/DeepSeek-V3.1";
    let model_pub_key = get_model_public_key(&server, model, Some("ecdsa"))
        .await
        .expect("Failed to fetch model public key from attestation report");

    // Mock ECDSA public key (128 hex characters = 64 bytes - uncompressed point)
    let mock_ecdsa_pub_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let request_body = serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "user",
                "content": "Hello, how are you?"
            }
        ],
        "stream": false,
        "max_tokens": 50
    });

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("X-Signing-Algo", "ecdsa")
        .add_header("X-Client-Pub-Key", mock_ecdsa_pub_key)
        .add_header("X-Model-Pub-Key", model_pub_key)
        .json(&request_body)
        .await;

    // Request should succeed (headers are passed through, vllm-proxy handles encryption)
    assert_eq!(
        response.status_code(),
        200,
        "Request with ECDSA encryption headers should succeed"
    );

    let response_json: serde_json::Value = response.json();
    assert!(
        response_json.get("choices").is_some(),
        "Response should contain choices"
    );
}

/// Test that chat completions with Ed25519 encryption headers are passed through correctly
#[tokio::test]
async fn test_chat_completions_with_ed25519_encryption_headers() {
    let (server, _guard) = setup_test_server().await;
    setup_deepseek_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Fetch model public key from attestation report
    let model = "deepseek-ai/DeepSeek-V3.1";
    let model_pub_key = get_model_public_key(&server, model, Some("ed25519"))
        .await
        .expect("Failed to fetch model public key from attestation report");

    // Mock Ed25519 public key (64 hex characters = 32 bytes)
    let mock_ed25519_pub_key = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    let request_body = serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "user",
                "content": "What is the weather?"
            }
        ],
        "stream": false,
        "max_tokens": 50
    });

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("X-Signing-Algo", "ed25519")
        .add_header("X-Client-Pub-Key", mock_ed25519_pub_key)
        .add_header("X-Model-Pub-Key", model_pub_key)
        .json(&request_body)
        .await;

    // Request should succeed (headers are passed through, vllm-proxy handles encryption)
    assert_eq!(
        response.status_code(),
        200,
        "Request with Ed25519 encryption headers should succeed"
    );

    let response_json: serde_json::Value = response.json();
    assert!(
        response_json.get("choices").is_some(),
        "Response should contain choices"
    );
}

/// Test that streaming chat completions with encryption headers work correctly
#[tokio::test]
async fn test_streaming_chat_completions_with_encryption_headers() {
    let (server, _guard) = setup_test_server().await;
    setup_deepseek_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Fetch model public key from attestation report
    let model = "deepseek-ai/DeepSeek-V3.1";
    let model_pub_key = get_model_public_key(&server, model, Some("ecdsa"))
        .await
        .expect("Failed to fetch model public key from attestation report");

    // Mock ECDSA public key (128 hex characters = 64 bytes - uncompressed point)
    let mock_ecdsa_pub_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let request_body = serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "user",
                "content": "Tell me a story"
            }
        ],
        "stream": true,
        "max_tokens": 50
    });

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("X-Signing-Algo", "ecdsa")
        .add_header("X-Client-Pub-Key", mock_ecdsa_pub_key)
        .add_header("X-Model-Pub-Key", model_pub_key)
        .json(&request_body)
        .await;

    // Request should succeed
    assert_eq!(
        response.status_code(),
        200,
        "Streaming request with encryption headers should succeed"
    );

    // Verify we get SSE stream
    let response_text = response.text();
    assert!(
        response_text.contains("data: "),
        "Response should contain SSE data chunks"
    );

    // Parse and verify stream structure
    let mut found_chunk = false;
    for line in response_text.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if data.trim() == "[DONE]" {
                break;
            }
            if let Ok(StreamChunk::Chat(_)) = serde_json::from_str::<StreamChunk>(data.trim()) {
                found_chunk = true;
                break;
            }
        }
    }
    assert!(
        found_chunk,
        "Should have received at least one valid SSE chunk"
    );
}

/// Test that requests without encryption headers still work (backward compatibility)
#[tokio::test]
async fn test_chat_completions_without_encryption_headers() {
    let (server, _guard) = setup_test_server().await;
    setup_deepseek_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let request_body = serde_json::json!({
        "model": "deepseek-ai/DeepSeek-V3.1",
        "messages": [
            {
                "role": "user",
                "content": "Hello, world!"
            }
        ],
        "stream": false,
        "max_tokens": 50
    });

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        // No encryption headers - should work normally
        .json(&request_body)
        .await;

    // Request should succeed without encryption headers
    assert_eq!(
        response.status_code(),
        200,
        "Request without encryption headers should succeed"
    );

    let response_json: serde_json::Value = response.json();
    assert!(
        response_json.get("choices").is_some(),
        "Response should contain choices"
    );
}

/// Test that requests with only one encryption header are handled gracefully
#[tokio::test]
async fn test_chat_completions_with_partial_encryption_headers() {
    let (server, _guard) = setup_test_server().await;
    setup_deepseek_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let model = "deepseek-ai/DeepSeek-V3.1";
    let model_pub_key = get_model_public_key(&server, model, Some("ecdsa"))
        .await
        .expect("Failed to fetch model public key from attestation report");

    // Mock ECDSA public key (128 hex characters = 64 bytes - uncompressed point)
    let mock_pub_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let request_body = serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "user",
                "content": "Test message"
            }
        ],
        "stream": false,
        "max_tokens": 50
    });

    // Test with only X-Signing-Algo (missing X-Client-Pub-Key)
    let response1 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("X-Signing-Algo", "ecdsa")
        .add_header("X-Model-Pub-Key", &model_pub_key)
        // Missing X-Client-Pub-Key
        .json(&request_body)
        .await;

    // Should still work (headers are passed through, vllm-proxy will handle validation)
    // The actual behavior depends on vllm-proxy implementation
    assert!(
        response1.status_code() == 200,
        "Request with partial encryption headers should succeed without encryption"
    );

    // Test with only X-Client-Pub-Key (missing X-Signing-Algo)
    let response2 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("X-Client-Pub-Key", mock_pub_key)
        .add_header("X-Model-Pub-Key", &model_pub_key)
        // Missing X-Signing-Algo
        .json(&request_body)
        .await;

    // Should still work (headers are passed through, vllm-proxy will handle validation)
    assert!(
        response2.status_code() == 200,
        "Request with partial encryption headers should succeed without encryption"
    );
}

/// Test that case-insensitive header names work correctly
#[tokio::test]
async fn test_chat_completions_with_case_insensitive_encryption_headers() {
    let (server, _guard) = setup_test_server().await;
    setup_deepseek_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let model = "deepseek-ai/DeepSeek-V3.1";
    let model_pub_key = get_model_public_key(&server, model, Some("ecdsa"))
        .await
        .expect("Failed to fetch model public key from attestation report");

    // Mock ECDSA public key (128 hex characters = 64 bytes - uncompressed point)
    let mock_pub_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let request_body = serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "user",
                "content": "Test message"
            }
        ],
        "stream": false,
        "max_tokens": 50
    });

    // Test with lowercase header names (Axum normalizes headers, but test both)
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("x-signing-algo", "ecdsa") // lowercase
        .add_header("x-client-pub-key", mock_pub_key) // lowercase
        .add_header("x-model-pub-key", &model_pub_key) // lowercase
        .json(&request_body)
        .await;

    // Should work (Axum normalizes header names to lowercase)
    assert_eq!(
        response.status_code(),
        200,
        "Request with lowercase encryption headers should succeed"
    );
}

/// Test that invalid encryption algorithm values are passed through
/// (vllm-proxy will validate and return appropriate error)
#[tokio::test]
async fn test_chat_completions_with_invalid_encryption_algorithm() {
    let (server, _guard) = setup_test_server().await;
    setup_deepseek_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let model = "deepseek-ai/DeepSeek-V3.1";
    let model_pub_key = get_model_public_key(&server, model, None)
        .await
        .expect("Failed to fetch model public key from attestation report");

    let mock_pub_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let request_body = serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "user",
                "content": "Test message"
            }
        ],
        "stream": false,
        "max_tokens": 50
    });

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("X-Signing-Algo", "invalid-algorithm")
        .add_header("X-Client-Pub-Key", mock_pub_key)
        .add_header("X-Model-Pub-Key", model_pub_key)
        .json(&request_body)
        .await;

    // Headers are passed through, vllm-proxy will validate and return error
    // The actual status code depends on vllm-proxy's validation
    assert!(
        response.status_code() == 200 || response.status_code() == 400,
        "Request with invalid algorithm should either succeed or return 400"
    );
}

/// Test that streaming responses with ECDSA encryption headers are passed through correctly
#[tokio::test]
async fn test_responses_streaming_with_ecdsa_encryption_headers() {
    let (server, _guard) = setup_test_server().await;
    setup_deepseek_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Fetch model public key from attestation report
    let model = "deepseek-ai/DeepSeek-V3.1";
    let model_pub_key = get_model_public_key(&server, model, Some("ecdsa"))
        .await
        .expect("Failed to fetch model public key from attestation report");

    // Mock ECDSA public key (128 hex characters = 64 bytes - uncompressed point)
    let mock_ecdsa_pub_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let request_body = serde_json::json!({
        "model": model,
        "input": "Hello, how are you?",
        "stream": true,
        "max_output_tokens": 50
    });

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("X-Signing-Algo", "ecdsa")
        .add_header("X-Client-Pub-Key", mock_ecdsa_pub_key)
        .add_header("X-Model-Pub-Key", model_pub_key)
        .json(&request_body)
        .await;

    // Request should succeed (headers are passed through, vllm-proxy handles encryption)
    assert_eq!(
        response.status_code(),
        200,
        "Streaming request with ECDSA encryption headers should succeed"
    );

    // Verify we get SSE stream
    let response_text = response.text();
    assert!(
        response_text.contains("event: "),
        "Response should contain SSE events"
    );
    assert!(
        response_text.contains("data: "),
        "Response should contain SSE data chunks"
    );
}

/// Test that streaming responses with Ed25519 encryption headers are passed through correctly
#[tokio::test]
async fn test_responses_streaming_with_ed25519_encryption_headers() {
    let (server, _guard) = setup_test_server().await;
    setup_deepseek_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Fetch model public key from attestation report
    let model = "deepseek-ai/DeepSeek-V3.1";
    let model_pub_key = get_model_public_key(&server, model, Some("ed25519"))
        .await
        .expect("Failed to fetch model public key from attestation report");

    // Mock Ed25519 public key (64 hex characters = 32 bytes)
    let mock_ed25519_pub_key = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    let request_body = serde_json::json!({
        "model": model,
        "input": "What is the weather?",
        "stream": true,
        "max_output_tokens": 50
    });

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("X-Signing-Algo", "ed25519")
        .add_header("X-Client-Pub-Key", mock_ed25519_pub_key)
        .add_header("X-Model-Pub-Key", model_pub_key)
        .json(&request_body)
        .await;

    // Request should succeed (headers are passed through, vllm-proxy handles encryption)
    assert_eq!(
        response.status_code(),
        200,
        "Streaming request with Ed25519 encryption headers should succeed"
    );

    // Verify we get SSE stream
    let response_text = response.text();
    assert!(
        response_text.contains("event: "),
        "Response should contain SSE events"
    );
    assert!(
        response_text.contains("data: "),
        "Response should contain SSE data chunks"
    );
}

/// Test that responses without encryption headers still work (backward compatibility)
#[tokio::test]
async fn test_responses_without_encryption_headers() {
    let (server, _guard) = setup_test_server().await;
    setup_deepseek_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let request_body = serde_json::json!({
        "model": "deepseek-ai/DeepSeek-V3.1",
        "input": "Hello, world!",
        "stream": false,
        "max_output_tokens": 50
    });

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        // No encryption headers - should work normally
        .json(&request_body)
        .await;

    // Request should succeed without encryption headers
    assert_eq!(
        response.status_code(),
        200,
        "Request without encryption headers should succeed"
    );

    let response_json: serde_json::Value = response.json();
    assert!(
        response_json.get("output").is_some(),
        "Response should contain output"
    );
}

/// Test that responses with only one encryption header are handled gracefully
#[tokio::test]
async fn test_responses_with_partial_encryption_headers() {
    let (server, _guard) = setup_test_server().await;
    setup_deepseek_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let model = "deepseek-ai/DeepSeek-V3.1";
    let model_pub_key = get_model_public_key(&server, model, Some("ecdsa"))
        .await
        .expect("Failed to fetch model public key from attestation report");

    // Mock ECDSA public key (128 hex characters = 64 bytes - uncompressed point)
    let mock_pub_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let request_body = serde_json::json!({
        "model": model,
        "input": "Test message",
        "stream": true, // Streaming mode required for encryption
        "max_output_tokens": 50
    });

    // Test with only X-Signing-Algo (missing X-Client-Pub-Key)
    let response1 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("X-Signing-Algo", "ecdsa")
        .add_header("X-Model-Pub-Key", &model_pub_key)
        // Missing X-Client-Pub-Key
        .json(&request_body)
        .await;

    // Should still work (headers are passed through, vllm-proxy will handle validation)
    // The actual behavior depends on vllm-proxy implementation
    assert!(
        response1.status_code() == 200,
        "Request with partial encryption headers should succeed without encryption"
    );

    // Test with only X-Client-Pub-Key (missing X-Signing-Algo)
    let response2 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("X-Client-Pub-Key", mock_pub_key)
        .add_header("X-Model-Pub-Key", &model_pub_key)
        // Missing X-Signing-Algo
        .json(&request_body)
        .await;

    // Should still work (headers are passed through, vllm-proxy will handle validation)
    assert!(
        response2.status_code() == 200,
        "Request with partial encryption headers should succeed without encryption"
    );
}

/// Test that case-insensitive header names work correctly for Responses API
#[tokio::test]
async fn test_responses_with_case_insensitive_encryption_headers() {
    let (server, _guard) = setup_test_server().await;
    setup_deepseek_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let model = "deepseek-ai/DeepSeek-V3.1";
    let model_pub_key = get_model_public_key(&server, model, Some("ecdsa"))
        .await
        .expect("Failed to fetch model public key from attestation report");

    // Mock ECDSA public key (128 hex characters = 64 bytes - uncompressed point)
    let mock_pub_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let request_body = serde_json::json!({
        "model": model,
        "input": {
            "type": "text",
            "text": "Test message"
        },
        "stream": true,
        "max_output_tokens": 50
    });

    // Test with lowercase header names (Axum normalizes headers, but test both)
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("x-signing-algo", "ecdsa") // lowercase
        .add_header("x-client-pub-key", mock_pub_key) // lowercase
        .add_header("x-model-pub-key", &model_pub_key) // lowercase
        .json(&request_body)
        .await;

    // Should work (Axum normalizes header names to lowercase)
    assert_eq!(
        response.status_code(),
        200,
        "Request with lowercase encryption headers should succeed"
    );
}

/// Test that invalid encryption algorithm values are handled correctly
#[tokio::test]
async fn test_responses_with_invalid_encryption_algorithm() {
    let (server, _guard) = setup_test_server().await;
    setup_deepseek_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let model = "deepseek-ai/DeepSeek-V3.1";
    let model_pub_key = get_model_public_key(&server, model, None)
        .await
        .expect("Failed to fetch model public key from attestation report");

    let mock_pub_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let request_body = serde_json::json!({
        "model": model,
        "input": "Test message",
        "stream": true,
        "max_output_tokens": 50
    });

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("X-Signing-Algo", "invalid-algorithm")
        .add_header("X-Client-Pub-Key", mock_pub_key)
        .add_header("X-Model-Pub-Key", model_pub_key)
        .json(&request_body)
        .await;

    // Should return 400 Bad Request because invalid algorithm is validated in cloud-api
    assert_eq!(
        response.status_code(),
        400,
        "Request with invalid algorithm should return 400 Bad Request"
    );

    let response_json: serde_json::Value = response.json();
    assert_eq!(
        response_json["error"]["type"], "invalid_parameter",
        "Error type should be 'invalid_parameter'"
    );
    assert!(
        response_json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Invalid X-Signing-Algo"),
        "Error message should indicate invalid signing algorithm"
    );
}

/// Test that non-streaming mode with encryption returns an error
/// Encryption requires streaming mode because encrypted chunks cannot be concatenated
#[tokio::test]
async fn test_responses_non_streaming_with_encryption_returns_error() {
    let (server, _guard) = setup_test_server().await;
    setup_deepseek_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let model = "deepseek-ai/DeepSeek-V3.1";
    let model_pub_key = get_model_public_key(&server, model, Some("ecdsa"))
        .await
        .expect("Failed to fetch model public key from attestation report");

    // Mock ECDSA public key (128 hex characters = 64 bytes - uncompressed point)
    let mock_pub_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let request_body = serde_json::json!({
        "model": model,
        "input": "Test message",
        "stream": false, // Non-streaming mode
        "max_output_tokens": 50
    });

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("X-Signing-Algo", "ecdsa")
        .add_header("X-Client-Pub-Key", mock_pub_key)
        .add_header("X-Model-Pub-Key", model_pub_key)
        .json(&request_body)
        .await;

    // Should return 400 Bad Request with clear error message
    assert_eq!(
        response.status_code(),
        400,
        "Non-streaming mode with encryption should return 400 Bad Request"
    );

    let response_json: serde_json::Value = response.json();
    assert_eq!(
        response_json["error"]["type"], "encryption_requires_streaming",
        "Error type should be 'encryption_requires_streaming'"
    );
    assert!(
        response_json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Non-streaming mode is not supported with encryption"),
        "Error message should explain that non-streaming mode is not supported with encryption"
    );
}
