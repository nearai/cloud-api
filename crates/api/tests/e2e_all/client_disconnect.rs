//! Client-disconnect coverage for the supported Chat Completions API.
//!
//! A missing signature of the caller's own disconnected stream is reported as
//! `STREAM_DISCONNECTED`; for any other caller it is not found, with no reason
//! attached.

use crate::common::*;

#[tokio::test]
async fn disconnected_chat_completion_without_signature_is_stream_disconnected_for_its_owner() {
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

    let client = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    // The usage row is written asynchronously after the stream ends.
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
    loop {
        let updated = client
            .execute(
                "UPDATE organization_usage_log SET stop_reason = 'client_disconnect' WHERE provider_request_id = $1",
                &[&completion_id],
            )
            .await
            .expect("Failed to update stop_reason");
        if updated > 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "usage row was not written"
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    client
        .execute(
            "DELETE FROM chat_signatures WHERE chat_id = $1",
            &[&completion_id],
        )
        .await
        .expect("Failed to delete signature");

    let signature_resp = server
        .get(&format!("/v1/signature/{completion_id}?signing_algo=ecdsa"))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(
        signature_resp.status_code(),
        200,
        "the caller's own disconnected stream reports why there is no signature"
    );
    let signature_json: serde_json::Value = signature_resp.json();
    assert_eq!(
        signature_json
            .get("error_code")
            .and_then(|code| code.as_str()),
        Some("STREAM_DISCONNECTED")
    );

    // Another organization asking for the same id learns nothing.
    let other_org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let other_api_key = get_api_key_for_org(&server, other_org.id).await;
    let other_resp = server
        .get(&format!("/v1/signature/{completion_id}?signing_algo=ecdsa"))
        .add_header("Authorization", format!("Bearer {other_api_key}"))
        .await;
    assert_eq!(other_resp.status_code(), 404);
    let other_json: serde_json::Value = other_resp.json();
    assert!(
        other_json.get("error_code").is_none(),
        "no disconnect reason is reported to another organization"
    );
}
