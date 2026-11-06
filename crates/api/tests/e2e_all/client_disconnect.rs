//! Client-disconnect coverage for the supported Chat Completions API.
//!
//! Stateless Responses preserve gateway attestation only after a completed
//! response. A disconnected Responses stream creates no `resp_*` attestation
//! record or legacy disconnect fallback because it has no persisted response
//! ID, so this module keeps the supported Chat Completions fallback coverage.

use crate::common::*;

#[tokio::test]
async fn chat_completion_signature_returns_stream_disconnected_on_client_disconnect() {
    let (server, _pool, mock, database) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    use crate::common::mock_prompts;

    let full_response = "Machine learning is a fascinating field of artificial intelligence today";
    let prompt = mock_prompts::build_prompt("Tell me about AI");
    mock.when(inference_providers::mock::RequestMatcher::ExactPrompt(
        prompt,
    ))
    .respond_with(
        inference_providers::mock::ResponseTemplate::new(full_response).with_disconnect_after(5),
    )
    .await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [{"role": "user", "content": "Tell me about AI"}],
            "stream": true
        }))
        .await;
    assert_eq!(response.status_code(), 200);

    let response_text = response.text();
    let completion_id = response_text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .find_map(|data| {
            serde_json::from_str::<serde_json::Value>(data)
                .ok()
                .and_then(|json| json.get("id").and_then(|id| id.as_str()).map(str::to_owned))
        })
        .expect("Should have completion_id from stream");

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let client = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    client
        .execute(
            "DELETE FROM chat_signatures WHERE chat_id = $1",
            &[&completion_id],
        )
        .await
        .expect("Failed to delete signature");
    client
        .execute(
            "UPDATE organization_usage_log SET stop_reason = 'client_disconnect' WHERE provider_request_id = $1",
            &[&completion_id],
        )
        .await
        .expect("Failed to update stop_reason");

    let signature_resp = server
        .get(&format!("/v1/signature/{completion_id}?signing_algo=ecdsa"))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        signature_resp.status_code(),
        200,
        "Signature endpoint should return 200 for client disconnect on chat completion. Response: {}",
        signature_resp.text()
    );

    let signature_json: serde_json::Value = signature_resp.json();
    assert_eq!(
        signature_json.get("error_code").and_then(|v| v.as_str()),
        Some("STREAM_DISCONNECTED")
    );
    assert_eq!(
        signature_json.get("message").and_then(|v| v.as_str()),
        Some("Verification not available due to disconnection.")
    );
}

/// Truncate the actual upstream stream, with no final usage or DONE frame.
#[tokio::test]
async fn interrupted_chat_retains_its_entire_budget_without_settling_partial_usage() {
    let (server, _pool, mock, database) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let org_id = uuid::Uuid::parse_str(&org.id).unwrap();
    let api_key = get_api_key_for_org(&server, org.id).await;
    mock.set_default_response(
        inference_providers::mock::ResponseTemplate::new("one two three four five six seven")
            .with_disconnect_after(2),
    )
    .await;
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": E2E_QWEN_MODEL_NAME,
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": true
        }))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert!(
        response.text().contains("one"),
        "the truncated mock must have executed"
    );
    let row = database
        .pool()
        .get()
        .await
        .unwrap()
        .query_one(
            "SELECT count(*), sum(amount)::bigint, reserved_credit($1),
            (SELECT count(*) FROM organization_usage_log WHERE organization_id=$1)
         FROM usage_budget_reservations
         WHERE organization_id=$1 AND state='dispatched' AND actual_cost IS NULL",
            &[&org_id],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 1);
    let held: i64 = row.get(1);
    assert!(held > 0);
    assert_eq!(row.get::<_, i64>(2), held);
    assert_eq!(
        row.get::<_, i64>(3),
        0,
        "partial usage must remain unsettled"
    );
}
