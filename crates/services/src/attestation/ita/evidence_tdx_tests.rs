use base64::{engine::general_purpose::STANDARD, Engine as _};
use config::{ItaEffectivePolicy, ItaPolicyIds, ItaTokenSigningAlg};
use serde_json::{json, Value};

use super::evidence_test_support::{
    effective_policy, gateway_quote, gateway_request, runtime_data, verifier_nonce, TestResult,
    DSTACK_EVENT_LOG, POLICY_A, POLICY_B,
};
use super::*;

pub(super) fn expected_tdx_json() -> Value {
    json!({
        "quote": "AQIDBA==",
        "runtime_data": STANDARD.encode(runtime_data()),
        "event_log": STANDARD.encode(DSTACK_EVENT_LOG.as_bytes()),
        "verifier_nonce": {
            "val": "dmVyaWZpZXItdmFsdWU=",
            "iat": "aWF0LWJ5dGVz",
            "signature": "dmVyaWZpZXItc2lnbmF0dXJl"
        }
    })
}

#[test]
fn maps_gateway_tdx_only_to_exact_ita_json() -> TestResult {
    // Given: gateway TDX evidence whose report_data matches the ITA nonce binding.
    let runtime_data = runtime_data();
    let gateway = gateway_quote(&runtime_data);

    // When: the evidence is mapped to an ITA attest request.
    let request = build_gateway_attest_request(ItaGatewayEvidenceInput {
        gateway: &gateway,
        verifier_nonce: &verifier_nonce(),
        policy: effective_policy(&format!("{POLICY_A},{POLICY_B}"))?,
    })?;

    // Then: the request is the ITA v2 TDX shape with base64 byte fields.
    assert_eq!(
        serde_json::to_value(request)?,
        json!({
            "policy_ids": [POLICY_A, POLICY_B],
            "token_signing_alg": "RS256",
            "policy_must_match": true,
            "tdx": expected_tdx_json()
        })
    );
    Ok(())
}

#[test]
fn omits_empty_policy_ids_but_keeps_effective_policy_controls() -> TestResult {
    // Given: effective policy has no policy IDs and default signing controls.
    let runtime_data = runtime_data();
    let gateway = gateway_quote(&runtime_data);

    // When: the gateway request is serialized.
    let value = serde_json::to_value(build_gateway_attest_request(ItaGatewayEvidenceInput {
        gateway: &gateway,
        verifier_nonce: &verifier_nonce(),
        policy: ItaEffectivePolicy {
            policy_ids: ItaPolicyIds::default(),
            policy_must_match: false,
            token_signing_alg: ItaTokenSigningAlg::Ps384,
        },
    })?)?;

    // Then: Task 1 DTO behavior omits empty policy_ids and keeps the other effective controls.
    assert_eq!(value.get("policy_ids"), None);
    assert_eq!(value["token_signing_alg"], "PS384");
    assert_eq!(value["policy_must_match"], false);
    Ok(())
}

#[test]
fn trims_event_log_before_base64_encoding() -> TestResult {
    let runtime_data = runtime_data();
    let mut gateway = gateway_quote(&runtime_data);
    gateway.event_log = format!(" \n{DSTACK_EVENT_LOG}\t");

    let value = serde_json::to_value(gateway_request(&gateway)?)?;

    assert_eq!(
        value["tdx"]["event_log"],
        "W3siaW1yIjozLCJldmVudF90eXBlIjoxLCJkaWdlc3QiOiIwMCJ9XQ=="
    );
    Ok(())
}

#[test]
fn omits_empty_event_log_placeholders() -> TestResult {
    let runtime_data = runtime_data();

    for event_log in ["", " \n\t", "0x", " 0x \n"] {
        let mut gateway = gateway_quote(&runtime_data);
        gateway.event_log = event_log.to_string();

        let value = serde_json::to_value(gateway_request(&gateway)?)?;

        assert_eq!(value["tdx"].get("event_log"), None, "{event_log:?}");
    }
    Ok(())
}

#[test]
fn report_data_uses_decoded_nonce_bytes_not_base64_text() -> TestResult {
    // Given: gateway report_data was produced with decoded Val || Iat bytes.
    let runtime_data = runtime_data();
    let gateway = gateway_quote(&runtime_data);

    // When: the evidence mapper verifies the TDX report data.
    let request = gateway_request(&gateway)?;

    // Then: verifier nonce signature is preserved and base64-text hashing would have failed.
    let value = serde_json::to_value(request)?;
    assert_eq!(
        value["tdx"]["verifier_nonce"]["signature"],
        "dmVyaWZpZXItc2lnbmF0dXJl"
    );
    Ok(())
}

#[test]
fn fails_closed_on_malformed_hex_quote() {
    // Given: a malformed gateway quote.
    let runtime_data = runtime_data();
    let mut gateway = gateway_quote(&runtime_data);
    gateway.intel_quote = "0xnot-hex".to_string();

    // When: the mapper parses the gateway evidence.
    let error = gateway_request(&gateway).expect_err("malformed quote must fail closed");

    // Then: the error identifies the quote field.
    assert!(matches!(
        error,
        ItaEvidenceError::InvalidHex {
            field: "tdx.quote",
            ..
        }
    ));
}

#[test]
fn fails_closed_on_missing_quote() {
    // Given: a gateway quote with no quote bytes.
    let runtime_data = runtime_data();
    let mut gateway = gateway_quote(&runtime_data);
    gateway.intel_quote.clear();

    // When: the mapper parses the gateway evidence.
    let error = gateway_request(&gateway).expect_err("missing quote must fail closed");

    // Then: the error identifies the missing TDX quote.
    assert!(matches!(error, ItaEvidenceError::MissingField("tdx.quote")));
}
