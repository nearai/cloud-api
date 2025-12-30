// Integration test that uses the real inference provider pool to validate signature metadata.
mod common;

use api::routes::attestation::{AttestationResponse, SignatureResponse};
use common::*;

use inference_providers::StreamChunk;

#[tokio::test]
async fn real_test_signature_signing_address_matches_model_attestation() {
    let (server, _pool, _guard) = setup_test_server_with_real_provider().await;
    let model_name = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let request_body = serde_json::json!({
        "messages": [
            {
                "role": "user",
                "content": "Respond with a short sentence."
            }
        ],
        "stream": true,
        "model": model_name,
        "nonce": 42
    });

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&request_body)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Streaming request should succeed"
    );

    let response_text = response.text();
    let mut chat_id: Option<String> = None;

    for line in response_text.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if data.trim() == "[DONE]" {
                break;
            }
            if let Ok(StreamChunk::Chat(chat_chunk)) = serde_json::from_str::<StreamChunk>(data) {
                if chat_id.is_none() {
                    chat_id = Some(chat_chunk.id.clone());
                }
            }
        }
    }

    let chat_id = chat_id.expect("Should extract chat_id from SSE stream");

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let signature_response = server
        .get(format!("/v1/signature/{chat_id}?model={model_name}&signing_algo=ecdsa").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        signature_response.status_code(),
        200,
        "Signature endpoint should return success"
    );

    let signature = signature_response.json::<SignatureResponse>();
    let signing_address = signature.signing_address;
    assert!(
        !signing_address.is_empty(),
        "Signing address should not be empty"
    );

    let encoded_model =
        url::form_urlencoded::byte_serialize(model_name.as_bytes()).collect::<String>();
    let attestation_response = server
        .get(format!("/v1/attestation/report?model={encoded_model}&signing_algo=ecdsa").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        attestation_response.status_code(),
        200,
        "Attestation report should return successfully"
    );

    let attestation = attestation_response.json::<AttestationResponse>();
    let attestation_addresses: Vec<String> = attestation
        .model_attestations
        .iter()
        .filter_map(|attestation| {
            attestation
                .get("signing_address")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string())
        })
        .collect();

    assert!(
        !attestation_addresses.is_empty(),
        "Model attestation list must contain at least one signing_address"
    );

    let normalized_signature_address = signing_address.trim_start_matches("0x").to_lowercase();
    let normalized_attestation_addresses: Vec<String> = attestation_addresses
        .iter()
        .map(|addr| addr.trim_start_matches("0x").to_lowercase())
        .collect();

    assert!(
        normalized_attestation_addresses
            .iter()
            .any(|addr| addr == &normalized_signature_address),
        "Signing address {signing_address} was not found in the model attestation list: {attestation_addresses:?}"
    );
}
