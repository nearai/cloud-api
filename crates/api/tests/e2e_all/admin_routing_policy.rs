use crate::common::*;

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
