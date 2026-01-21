// Real provider integration test verifying the response signature matches gateway attestation metadata.
mod common;

use common::*;
use endpoints::*;

#[tokio::test]
async fn real_test_signature_signing_address_matches_gateway_attestation_stream() {
    let (server, _pool, _db, _guard) = setup_test_server_real_providers().await;
    let model_name = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let request_body = serde_json::json!({
        "input": "Respond with only two words.",
        "temperature": 0.7,
        "max_output_tokens": 50,
        "stream": true,
        "model": model_name,
    });
    let (response_id, _raw_body) =
        create_response_stream_no_conversation_and_get_id(&server, &api_key, &request_body).await;

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let signature =
        get_signature_for_id(&server, &api_key, &response_id, &model_name, "ecdsa").await;
    let signing_address = signature.signing_address;
    assert!(
        !signing_address.is_empty(),
        "Signing address should not be empty"
    );

    let attestation = get_attestation_report(&server, &api_key, &model_name, "ecdsa").await;
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

#[tokio::test]
async fn real_test_signature_signing_address_matches_gateway_attestation_non_stream() {
    let (server, _pool, _db, _guard) = setup_test_server_real_providers().await;
    let model_name = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let request_body = serde_json::json!({
        "input": "Respond with only two words.",
        "temperature": 0.7,
        "max_output_tokens": 50,
        "stream": false,
        "model": model_name,
    });
    let (response_id, _raw_body) =
        create_response_non_stream_no_conversation_and_get_id(&server, &api_key, &request_body)
            .await;

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let signature =
        get_signature_for_id(&server, &api_key, &response_id, &model_name, "ecdsa").await;
    let signing_address = signature.signing_address;
    assert!(
        !signing_address.is_empty(),
        "Signing address should not be empty"
    );

    let attestation = get_attestation_report(&server, &api_key, &model_name, "ecdsa").await;
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
