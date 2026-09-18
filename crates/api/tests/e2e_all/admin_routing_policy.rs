use crate::common::*;
use inference_providers::ChatCompletionParams;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn admin_validates_mandatory_routing_before_persisting() {
    let (server, _) = setup_test_server_with_mock_web_search().await;
    let policy = serde_json::json!({"provider": {"zdr": true, "only": ["fireworks"]}});
    for (backend, enforcement) in [
        ("anthropic", policy.clone()),
        ("gemini", policy.clone()),
        (
            "openai_compatible",
            serde_json::json!({"model": "override"}),
        ),
        ("openai_compatible", serde_json::json!({"provider": null})),
        ("openai_compatible", serde_json::json!(false)),
    ] {
        let model_name = format!("invalid-policy-{}", uuid::Uuid::new_v4());
        let response = server
            .patch("/v1/admin/models")
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&serde_json::json!({&model_name: {
                "isActive": false,
                "modelDisplayName": "Synthetic routing policy test",
                "modelDescription": "Synthetic routing policy test",
                "contextLength": 4096,
                "inputCostPerToken": {"amount": 220, "currency": "USD"},
                "outputCostPerToken": {"amount": 660, "currency": "USD"},
                "providerType": "external",
                "providerConfig": {"backend": backend, "base_url": "https://example.com",
                    "enforced_request_body": enforcement}
            }}))
            .await;
        assert_eq!(response.status_code(), 400, "{}", response.text());
        assert!(response.text().contains("enforced_request_body"));
        let models = server
            .get("/v1/admin/models")
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .await;
        models.assert_status_ok();
        let body: serde_json::Value = models.json();
        assert!(!body["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["modelId"] == model_name));
    }

    let missing_base_url_model = format!("invalid-schema-{}", uuid::Uuid::new_v4());
    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({&missing_base_url_model: {
            "isActive": false,
            "modelDisplayName": "Synthetic schema validation test",
            "modelDescription": "Synthetic schema validation test",
            "contextLength": 4096,
            "inputCostPerToken": {"amount": 220, "currency": "USD"},
            "outputCostPerToken": {"amount": 660, "currency": "USD"},
            "providerType": "external",
            "providerConfig": {"backend": "openai_compatible",
                "enforced_request_body": {"provider": {"zdr": true}}}
        }}))
        .await;
    assert_eq!(response.status_code(), 400, "{}", response.text());
    assert!(response.text().contains("backend schema"));
    let models = server
        .get("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    models.assert_status_ok();
    let body: serde_json::Value = models.json();
    assert!(!body["models"]
        .as_array()
        .unwrap()
        .iter()
        .any(|model| model["modelId"] == missing_base_url_model));

    let model_name = format!("valid-policy-{}", uuid::Uuid::new_v4());
    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({&model_name: {
            "isActive": false,
            "modelDisplayName": "Synthetic routing policy test",
            "modelDescription": "Synthetic routing policy test",
            "contextLength": 4096,
            "inputCostPerToken": {"amount": 220, "currency": "USD"},
            "outputCostPerToken": {"amount": 660, "currency": "USD"},
            "providerType": "external",
            "providerConfig": {"backend": "openai_compatible", "base_url": "https://example.com",
                "enforced_request_body": policy}
        }}))
        .await;
    response.assert_status_ok();
    let body: serde_json::Value = response.json();
    assert_eq!(
        body[0]["metadata"]["providerConfig"]["enforced_request_body"],
        policy
    );
}

#[tokio::test]
async fn partial_external_config_update_reloads_the_live_provider() {
    let (server, pool, _, _) = setup_test_server_with_pool_and_config(|config| {
        config.external_providers.timeout_seconds = 5;
    })
    .await;
    let old_upstream = MockServer::start().await;
    let new_upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "synthetic",
            "object": "chat.completion",
            "created": 0,
            "model": "synthetic/upstream",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "42"},
                "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .expect(1)
        .mount(&new_upstream)
        .await;

    let model_name = format!("external/partial-policy-{}", uuid::Uuid::new_v4());
    let create = serde_json::json!({&model_name: {
        "isActive": true,
        "modelDisplayName": "Synthetic partial policy test",
        "modelDescription": "Synthetic partial policy test",
        "contextLength": 4096,
        "inputCostPerToken": {"amount": 220, "currency": "USD"},
        "outputCostPerToken": {"amount": 660, "currency": "USD"},
        "providerType": "external",
        "providerConfig": {"backend": "openai_compatible", "base_url": old_upstream.uri(),
            "api_key": "synthetic-test-key"}
    }});
    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&create)
        .await;
    response.assert_status_ok();
    assert!(pool.has_provider(&model_name).await);

    let policy = serde_json::json!({
        "only": ["novita/fp8"], "allow_fallbacks": false,
        "zdr": true, "data_collection": "deny"
    });
    let update = serde_json::json!({&model_name: {
        "providerConfig": {"backend": "openai_compatible", "base_url": new_upstream.uri(),
            "api_key": "synthetic-test-key", "enforced_request_body": {"provider": policy}}
    }});
    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&update)
        .await;
    response.assert_status_ok();

    let params: ChatCompletionParams = serde_json::from_value(serde_json::json!({
        "model": model_name,
        "messages": [{"role": "user", "content": "Reply only with 42"}],
        "provider": {"only": ["forbidden"], "allow_fallbacks": true,
            "zdr": false, "data_collection": "allow"}
    }))
    .unwrap();
    pool.chat_completion(params, "synthetic-request".into())
        .await
        .unwrap();

    assert!(old_upstream.received_requests().await.unwrap().is_empty());
    let requests = new_upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["provider"], policy);
}
