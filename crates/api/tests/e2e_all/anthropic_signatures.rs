//! Verify native Messages receipts through the public signature endpoint.
use crate::common::*;
use bytes::Bytes;
use serde_json::{json, Value};
use wiremock::{
    matchers::{body_partial_json, header, method, path},
    Mock, MockServer, ResponseTemplate,
};

#[tokio::test]
async fn json_message_has_gateway_receipts_and_correlated_billing() {
    verify_message(false).await;
}

#[tokio::test]
async fn streaming_message_has_gateway_receipts_and_correlated_billing() {
    verify_message(true).await;
}

async fn verify_message(stream: bool) {
    let upstream = MockServer::start().await;
    let id = format!("msg_{}", uuid::Uuid::new_v4());
    let raw = if stream {
        format!("event: message_start\r\ndata: {}\r\n\r\nevent: ping\ndata: {{\"type\":\"ping\"}}\n\nevent: content_block_start\ndata: {}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
            json!({"type":"message_start","message":{"id":id,"type":"message","content":[],"usage":{"input_tokens":10}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_fixture","name":"weather","input":{"city":"Tokyo"}}}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}))
    } else {
        serde_json::to_string_pretty(&json!({"id":id,"type":"message","role":"assistant","model":"claude-fixture",
            "content":[{"type":"tool_use","id":"toolu_fixture","name":"weather","input":{"city":"Tokyo"}}],
            "stop_reason":"tool_use","usage":{"input_tokens":10,"output_tokens":5}})).unwrap()
    };
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "fixture-upstream-key"))
        .and(body_partial_json(
            json!({"model":"claude-fixture","stream":stream}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("request-id", "req_fixture")
                .set_body_raw(
                    raw.clone(),
                    if stream {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                ),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let (server, _, _, database) = setup_test_server_with_pool_and_config(|config| {
        config.external_providers.enable_anthropic_messages = true;
        config.external_providers.timeout_seconds = 5;
    })
    .await;
    let model = format!("anthropic/messages-signatures-{}", uuid::Uuid::new_v4());
    let alias = format!("messages-alias-{}", uuid::Uuid::new_v4());
    let mut batch = api::models::BatchUpdateModelApiRequest::new();
    batch.insert(model.clone(), serde_json::from_value(json!({
        "inputCostPerToken":{"amount":1000,"currency":"USD"},
        "outputCostPerToken":{"amount":2000,"currency":"USD"},
        "modelDisplayName":"Messages fixture","modelDescription":"Receipt integration test",
        "contextLength":10000,"maxOutputLength":1000,"isActive":true,
        "aliases":[alias],"verifiable":false,"attestationSupported":false,
        "inputModalities":["text"],"outputModalities":["text"],
        "providerType":"external","providerConfig":{"backend":"anthropic",
            "base_url":format!("{}/v1",upstream.uri()),"model_name":"claude-fixture","api_key":"fixture-upstream-key"}
    })).unwrap());
    admin_batch_upsert_models(&server, batch, get_session_id()).await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let key = get_api_key_for_org(&server, org.id).await;
    // Pretty-printed input and a model alias detect accidental hashing after
    // JSON normalization or the provider's model-name rewrite.
    let request = serde_json::to_string_pretty(&json!({"model":alias,"max_tokens":16,
        "stream":stream,"messages":[{"role":"user","content":"Weather?"}],
        "tools":[{"name":"weather","input_schema":{"type":"object","properties":{"city":{"type":"string"}}}}]})).unwrap();
    let response = server
        .post("/v1/messages")
        .add_header("x-api-key", &key)
        .add_header("anthropic-version", "2023-06-01")
        .add_header("Content-Type", "application/json")
        .bytes(Bytes::from(request.clone()))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(response.text(), raw);
    assert_eq!(response.header("request-id"), "req_fixture");
    assert_eq!(response.header("x-serving-provider"), "non-attested");
    let inference_id = services::completions::hash_inference_id_to_uuid(&id);
    assert_eq!(
        response.header("Inference-Id"),
        inference_id.to_string().as_str()
    );
    assert!(!response.headers().contains_key("x-generation-id"));
    assert!(!response.headers().contains_key("x-signature-id"));

    let expected = format!("{}:{}", compute_sha256(&request), compute_sha256(&raw));
    for algorithm in ["ecdsa", "ed25519"] {
        // No polling: a completed response's successful receipt write must
        // already be visible, including for SSE.
        let signature_response = server
            .get(&format!("/v1/signature/{id}?signing_algo={algorithm}"))
            .add_header("Authorization", format!("Bearer {key}"))
            .await;
        assert_eq!(
            signature_response.status_code(),
            200,
            "{}",
            signature_response.text()
        );
        let receipt: Value = signature_response.json();
        assert_eq!(receipt["signature_kind"], "gateway");
        assert_eq!(receipt["text"], expected);
        let signature = receipt["signature"].as_str().unwrap();
        let signer = receipt["signing_address"].as_str().unwrap();
        assert!(if algorithm == "ecdsa" {
            verify_ecdsa_signature(&expected, signature, signer)
        } else {
            verify_ed25519_signature(&expected, signature, signer)
        });
    }
    let client = database.pool().get().await.unwrap();
    let rows = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let rows = client.query("SELECT provider_request_id, input_tokens, output_tokens FROM organization_usage_log WHERE inference_id = $1", &[&inference_id]).await.unwrap();
            if !rows.is_empty() { break rows; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>("provider_request_id"), id);
    assert_eq!(rows[0].get::<_, i32>("input_tokens"), 10);
    assert_eq!(rows[0].get::<_, i32>("output_tokens"), 5);
    upstream.verify().await;
}
