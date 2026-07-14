use serde_json::{json, Map, Value};

use super::evidence_test_support::{
    effective_policy, gateway_quote, gpu_nonce, model_evidence, model_request, runtime_data,
    verifier_nonce, TestResult, POLICY_A,
};
use super::tdx_tests::expected_tdx_json;
use super::*;

#[test]
fn maps_tdx_and_matching_nvgpu_to_exact_ita_json() -> TestResult {
    // Given: gateway TDX evidence and model GPU evidence bound to the ITA-derived nonce.
    let runtime_data = runtime_data();
    let gateway = gateway_quote(&runtime_data);
    let model_evidence = vec![model_evidence("HOPPER", &gpu_nonce())];

    // When: the combined request is built.
    let request = build_model_attest_request(ItaModelEvidenceInput {
        gateway: &gateway,
        model_attestations: &model_evidence,
        verifier_nonce: &verifier_nonce(),
        policy: effective_policy(POLICY_A)?,
    })?;

    // Then: the ITA request includes verifier nonce, derived GPU nonce, arch, and evidence list.
    assert_eq!(
        serde_json::to_value(request)?,
        json!({
            "policy_ids": [POLICY_A],
            "token_signing_alg": "RS256",
            "policy_must_match": true,
            "tdx": expected_tdx_json(),
            "nvgpu": {
                "verifier_nonce": {
                    "val": "dmVyaWZpZXItdmFsdWU=",
                    "iat": "aWF0LWJ5dGVz",
                    "signature": "dmVyaWZpZXItc2lnbmF0dXJl"
                },
                "gpu_nonce": gpu_nonce(),
                "arch": "HOPPER",
                "evidence_list": [{ "certificate": "Y2VydA==", "evidence": "ZXZpZGVuY2U=" }]
            }
        })
    );
    Ok(())
}

#[test]
fn gpu_nonce_uses_decoded_nonce_bytes_not_base64_text() -> TestResult {
    // Given: model GPU evidence is bound to SHA256(decoded Val || Iat).
    let runtime_data = runtime_data();
    let gateway = gateway_quote(&runtime_data);
    let model_evidence = vec![model_evidence("HOPPER", &gpu_nonce())];

    // When: the evidence mapper derives the ITA GPU nonce.
    let request = model_request(&gateway, &model_evidence)?;

    // Then: the output GPU nonce matches decoded-byte material and preserves verifier signature.
    let value = serde_json::to_value(request)?;
    assert_eq!(value["nvgpu"]["gpu_nonce"], gpu_nonce());
    assert_eq!(
        value["nvgpu"]["verifier_nonce"]["signature"],
        "dmVyaWZpZXItc2lnbmF0dXJl"
    );
    Ok(())
}

#[test]
fn fails_closed_on_missing_token_evidence() {
    // Given: GPU evidence missing the token evidence bytes.
    let runtime_data = runtime_data();
    let gateway = gateway_quote(&runtime_data);
    let payload = json!({"gpu_nonce": gpu_nonce(), "arch": "HOPPER", "evidence_list": [{"certificate": "Y2VydA=="}]});
    let mut evidence = Map::new();
    evidence.insert(
        "nvidia_payload".to_string(),
        Value::String(payload.to_string()),
    );

    // When: the model mapper parses the GPU evidence list.
    let error = model_request(&gateway, &[evidence]).expect_err("missing evidence must fail");

    // Then: the error identifies the missing evidence item.
    assert!(matches!(
        error,
        ItaEvidenceError::MissingField("nvgpu.evidence_list.evidence")
    ));
}

#[test]
fn fails_closed_on_missing_gpu_arch() {
    // Given: GPU evidence without an architecture.
    let runtime_data = runtime_data();
    let gateway = gateway_quote(&runtime_data);
    let payload = json!({"gpu_nonce": gpu_nonce(), "evidence_list": [{"certificate": "Y2VydA==", "evidence": "ZXZpZGVuY2U="}]});
    let mut evidence = Map::new();
    evidence.insert(
        "nvidia_payload".to_string(),
        Value::String(payload.to_string()),
    );

    // When: the model mapper parses the GPU evidence.
    let error = model_request(&gateway, &[evidence]).expect_err("missing GPU arch must fail");

    // Then: the error identifies the missing arch.
    assert!(matches!(
        error,
        ItaEvidenceError::MissingField("nvgpu.arch")
    ));
}

#[test]
fn fails_closed_on_inconsistent_gpu_arch() {
    // Given: model evidence entries disagree on the GPU architecture.
    let runtime_data = runtime_data();
    let gateway = gateway_quote(&runtime_data);
    let evidence = vec![
        model_evidence("HOPPER", &gpu_nonce()),
        model_evidence("BLACKWELL", &gpu_nonce()),
    ];

    // When: the model mapper combines evidence entries.
    let error = model_request(&gateway, &evidence).expect_err("mixed GPU arch must fail");

    // Then: the error preserves both architecture values for diagnosis.
    assert!(matches!(
        error,
        ItaEvidenceError::InconsistentGpuArch { .. }
    ));
}

#[test]
fn fails_closed_on_nonce_mismatch() {
    // Given: model GPU evidence bound to a nonce other than the ITA-derived nonce.
    let runtime_data = runtime_data();
    let gateway = gateway_quote(&runtime_data);
    let evidence = vec![model_evidence("HOPPER", &"00".repeat(32))];

    // When: the model mapper checks the GPU nonce binding.
    let error = model_request(&gateway, &evidence).expect_err("nonce mismatch must fail");

    // Then: the request is rejected instead of emitting gateway-only output.
    assert!(matches!(error, ItaEvidenceError::GpuNonceMismatch));
}

#[test]
fn fails_closed_on_unsupported_provider_evidence() {
    // Given: a Chutes-style report without an ITA GPU nonce binding.
    let runtime_data = runtime_data();
    let gateway = gateway_quote(&runtime_data);
    let mut evidence = Map::new();
    evidence.insert("intel_quote".to_string(), Value::String("0102".to_string()));
    evidence.insert(
        "nvidia_payload".to_string(),
        Value::String(json!({"arch":"HOPPER","evidence_list":[{"certificate":"Y2VydA==","evidence":"ZXZpZGVuY2U="}]}).to_string()),
    );

    // When: the ITA mapper cannot prove the GPU evidence used the ITA nonce.
    let error = model_request(&gateway, &[evidence]).expect_err("unsupported provider must fail");

    // Then: the model request fails closed rather than silently returning gateway-only output.
    assert!(matches!(
        error,
        ItaEvidenceError::UnsupportedProviderEvidence
    ));
}
