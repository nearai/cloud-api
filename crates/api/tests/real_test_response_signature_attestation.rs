// Real provider integration test verifying the response signature matches gateway attestation metadata.
mod common;

use api::routes::attestation::{AttestationResponse, SignatureResponse};
use common::*;

#[tokio::test]
async fn real_test_signature_signing_address_matches_gateway_attestation() {
    let (server, _pool, _guard) = setup_test_server_with_real_provider().await;
    let model_name = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let conversation_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "name": "Real Signature Gateway Attestation",
            "description": "Ensure gateway attestation matches response signature"
        }))
        .await;

    assert_eq!(
        conversation_response.status_code(),
        201,
        "Failed to create conversation"
    );

    let conversation = conversation_response.json::<api::models::ConversationObject>();

    let request_body = serde_json::json!({
        "conversation": {
            "id": conversation.id
        },
        "input": "Respond with only two words.",
        "temperature": 0.7,
        "max_output_tokens": 50,
        "stream": true,
        "model": model_name,
        "signing_algo": "ecdsa",
        "nonce": 42
    });

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&request_body)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Real response request should succeed"
    );

    let response_text = response.text();
    let mut response_id: Option<String> = None;

    for line_chunk in response_text.split("\n\n") {
        if line_chunk.trim().is_empty() {
            continue;
        }

        let mut event_type = "";
        let mut event_data = "";

        for line in line_chunk.lines() {
            if let Some(event_name) = line.strip_prefix("event: ") {
                event_type = event_name;
            } else if let Some(data) = line.strip_prefix("data: ") {
                event_data = data;
            }
        }

        if event_type == "response.created" && !event_data.is_empty() {
            if let Ok(event_json) = serde_json::from_str::<serde_json::Value>(event_data) {
                if response_id.is_none() {
                    if let Some(response_obj) = event_json.get("response") {
                        if let Some(id) = response_obj.get("id").and_then(|v| v.as_str()) {
                            response_id = Some(id.to_string());
                        }
                    }
                }
            }
        }
    }

    let response_id = response_id.expect("Should have extracted response_id");

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let signature_url =
        format!("/v1/signature/{response_id}?model={model_name}&signing_algo=ecdsa");
    let signature_response = server
        .get(&signature_url)
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        signature_response.status_code(),
        200,
        "Signature endpoint should succeed"
    );

    let signature = signature_response.json::<SignatureResponse>();
    let signing_address = signature.signing_address;
    assert!(
        !signing_address.is_empty(),
        "Signing address should not be empty"
    );

    let encoded_model =
        url::form_urlencoded::byte_serialize(model_name.as_bytes()).collect::<String>();
    let attestation_url =
        format!("/v1/attestation/report?model={encoded_model}&signing_algo=ecdsa");
    let attestation_response = server
        .get(&attestation_url)
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        attestation_response.status_code(),
        200,
        "Attestation report should succeed"
    );

    let attestation = attestation_response.json::<AttestationResponse>();
    let gateway_address = attestation.gateway_attestation.signing_address;
    assert!(
        !gateway_address.is_empty(),
        "Gateway attestation should expose signing_address"
    );

    let normalized_signature_address = signing_address.trim_start_matches("0x").to_lowercase();
    let normalized_gateway_address = gateway_address.trim_start_matches("0x").to_lowercase();

    assert_eq!(
        normalized_signature_address, normalized_gateway_address,
        "Signature signing address {signing_address} should match gateway attestation signing address {gateway_address}"
    );
}
