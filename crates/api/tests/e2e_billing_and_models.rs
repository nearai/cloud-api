// Import common test utilities
mod common;

use common::*;

#[tokio::test]
async fn test_billing_costs_happy_path() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Make a chat completion request (non-streaming to ensure usage is recorded immediately)
    let completion_response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [{"role": "user", "content": "Test"}],
            "max_tokens": 10,
            "stream": false
        }))
        .await;

    assert_eq!(
        completion_response.status_code(),
        200,
        "Chat completion failed: {}",
        completion_response.text()
    );

    // Extract Inference-Id header
    let inference_id = completion_response
        .headers()
        .get("Inference-Id")
        .expect("Missing Inference-Id header")
        .to_str()
        .unwrap();
    let real_inference_uuid = uuid::Uuid::parse_str(inference_id).unwrap();

    // Create a fake inference ID that doesn't exist
    let fake_inference_uuid = uuid::Uuid::new_v4();

    // Wait for async usage recording to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Query billing costs for both real and fake IDs
    let billing_response = server
        .post("/v1/billing/costs")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "requestIds": [real_inference_uuid, fake_inference_uuid]
        }))
        .await;

    assert_eq!(billing_response.status_code(), 200);

    let body: serde_json::Value = billing_response.json();
    let requests = body["requests"].as_array().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "Should return 2 cost entries (1 real + 1 missing)"
    );

    // Find the real and fake entries
    let real_entry = requests
        .iter()
        .find(|r| r["requestId"] == real_inference_uuid.to_string())
        .expect("Real inference ID should be in response");
    let fake_entry = requests
        .iter()
        .find(|r| r["requestId"] == fake_inference_uuid.to_string())
        .expect("Fake inference ID should be in response");

    // Verify real ID has positive cost
    assert!(
        real_entry["costNanoUsd"].as_i64().unwrap() > 0,
        "Real inference ID should have positive cost"
    );

    // Verify fake ID has zero cost
    assert_eq!(
        fake_entry["costNanoUsd"].as_i64().unwrap(),
        0,
        "Missing inference ID should have zero cost"
    );
}
