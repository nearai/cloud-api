// Import common test utilities
mod common;

use common::*;

use inference_providers::StreamChunk;

// ============================================
// End-to-End Encryption Tests
// ============================================
//
// These tests verify that encryption headers (X-Signing-Algo and X-Signing-Pub-Key)
// are properly extracted and passed through from cloud-api to vllm-proxy.
//
// Note: These tests are currently skipped because they require a running vllm-proxy
// instance with encryption support. To run these tests:
// 1. Start vllm-proxy with encryption enabled
// 2. Remove the #[ignore] attribute from the tests
// 3. Ensure the test environment has proper encryption keys configured

/// Test that chat completions with ECDSA encryption headers are passed through correctly
#[tokio::test]
#[ignore] // Skip by default - requires vllm-proxy with encryption support
async fn test_chat_completions_with_ecdsa_encryption_headers() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Mock ECDSA public key (64 hex characters = 32 bytes)
    let mock_ecdsa_pub_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let request_body = serde_json::json!({
        "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
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
        .add_header("X-Signing-Pub-Key", mock_ecdsa_pub_key)
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
#[ignore] // Skip by default - requires vllm-proxy with encryption support
async fn test_chat_completions_with_ed25519_encryption_headers() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Mock Ed25519 public key (64 hex characters = 32 bytes)
    let mock_ed25519_pub_key = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    let request_body = serde_json::json!({
        "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
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
        .add_header("X-Signing-Pub-Key", mock_ed25519_pub_key)
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
#[ignore] // Skip by default - requires vllm-proxy with encryption support
async fn test_streaming_chat_completions_with_encryption_headers() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Mock ECDSA public key
    let mock_ecdsa_pub_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let request_body = serde_json::json!({
        "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
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
        .add_header("X-Signing-Pub-Key", mock_ecdsa_pub_key)
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
            if let Ok(StreamChunk::Chat(_)) = serde_json::from_str::<StreamChunk>(data) {
                found_chunk = true;
                break;
            }
        }
    }
    assert!(found_chunk, "Should have received at least one valid SSE chunk");
}

/// Test that requests without encryption headers still work (backward compatibility)
#[tokio::test]
async fn test_chat_completions_without_encryption_headers() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let request_body = serde_json::json!({
        "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
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
#[ignore] // Skip by default - requires vllm-proxy with encryption support
async fn test_chat_completions_with_partial_encryption_headers() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let mock_pub_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let request_body = serde_json::json!({
        "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
        "messages": [
            {
                "role": "user",
                "content": "Test message"
            }
        ],
        "stream": false,
        "max_tokens": 50
    });

    // Test with only X-Signing-Algo (missing X-Signing-Pub-Key)
    let response1 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("X-Signing-Algo", "ecdsa")
        // Missing X-Signing-Pub-Key
        .json(&request_body)
        .await;

    // Should still work (headers are passed through, vllm-proxy will handle validation)
    // The actual behavior depends on vllm-proxy implementation
    assert!(
        response1.status_code() == 200 || response1.status_code() == 400,
        "Request with partial encryption headers should either succeed or return 400"
    );

    // Test with only X-Signing-Pub-Key (missing X-Signing-Algo)
    let response2 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("X-Signing-Pub-Key", mock_pub_key)
        // Missing X-Signing-Algo
        .json(&request_body)
        .await;

    // Should still work (headers are passed through, vllm-proxy will handle validation)
    assert!(
        response2.status_code() == 200 || response2.status_code() == 400,
        "Request with partial encryption headers should either succeed or return 400"
    );
}

/// Test that case-insensitive header names work correctly
#[tokio::test]
#[ignore] // Skip by default - requires vllm-proxy with encryption support
async fn test_chat_completions_with_case_insensitive_encryption_headers() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let mock_pub_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let request_body = serde_json::json!({
        "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
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
        .add_header("x-signing-pub-key", mock_pub_key) // lowercase
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
#[ignore] // Skip by default - requires vllm-proxy with encryption support
async fn test_chat_completions_with_invalid_encryption_algorithm() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id).await;

    let mock_pub_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let request_body = serde_json::json!({
        "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
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
        .add_header("X-Signing-Pub-Key", mock_pub_key)
        .json(&request_body)
        .await;

    // Headers are passed through, vllm-proxy will validate and return error
    // The actual status code depends on vllm-proxy's validation
    assert!(
        response.status_code() == 200 || response.status_code() == 400,
        "Request with invalid algorithm should either succeed or return 400"
    );
}

