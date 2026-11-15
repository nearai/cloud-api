use crate::common::*;
use serde_json::json;

/// A stored organization system prompt reaches the provider on a stateless
/// response. The prompt is written to the settings column directly: the route
/// that edited it is no longer served here, but inference still reads it.
#[tokio::test]
async fn test_system_prompt_integration_with_stateless_responses() {
    let (server, _pool, mock, database) = setup_test_server_with_pool().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    setup_glm_model(&server).await;
    setup_qwen_model(&server).await;

    let client = database.pool().get().await.unwrap();
    let org_id: uuid::Uuid = org.id.parse().unwrap();
    client
        .execute(
            "UPDATE organizations SET settings = jsonb_set(COALESCE(settings, '{}'::jsonb), \
             '{system_prompt}', to_jsonb($2::text)) WHERE id = $1",
            &[&org_id, &"You are a test assistant."],
        )
        .await
        .unwrap();

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input": "Hello",
            "store": false,
            "stream": false
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let response_json: serde_json::Value = response.json();
    assert_eq!(response_json["status"], "completed");

    let params = mock.last_chat_params().await.expect("provider was called");
    let system_text: String = params
        .messages
        .iter()
        .filter(|m| matches!(m.role, inference_providers::MessageRole::System))
        .filter_map(|m| m.content.as_ref().and_then(|c| c.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        system_text.starts_with("You are a test assistant."),
        "the stored organization prompt must lead the system message sent to the provider"
    );
}
