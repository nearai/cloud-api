use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Map, Value};

use super::evidence_test_support::{
    canonical_pem_chain_b64, effective_policy, gateway_quote, gpu_nonce, model_evidence,
    model_evidence_with_certificate, model_request, runtime_data, verifier_nonce, TestResult,
    POLICY_A, TEST_CERT_DER,
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
                "evidence_list": [{
                    "certificate": canonical_pem_chain_b64(&[TEST_CERT_DER]),
                    "evidence": "ZXZpZGVuY2U="
                }]
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

#[test]
fn normalizes_cert_chain_with_trailing_nul_padding() -> TestResult {
    // Given: a cert chain carrying NVML fixed-size-buffer NUL padding, as prod
    // providers emit. NRAS accepts that chain; ITA rejects the whole attest
    // request with a bare 400, so the mapper must canonicalize it.
    let runtime_data = runtime_data();
    let gateway = gateway_quote(&runtime_data);
    let canonical = canonical_pem_chain_b64(&[TEST_CERT_DER]);
    let mut dirty = STANDARD.decode(&canonical)?;
    dirty.extend_from_slice(b"\n\x00\x00\x00\x00\x00");
    let evidence = vec![model_evidence_with_certificate(
        "HOPPER",
        &gpu_nonce(),
        &STANDARD.encode(dirty),
    )];

    // When: the model request is built.
    let request = model_request(&gateway, &evidence)?;

    // Then: the padding is dropped and the chain is byte-identical to canonical PEM.
    let value = serde_json::to_value(request)?;
    assert_eq!(value["nvgpu"]["evidence_list"][0]["certificate"], canonical);
    Ok(())
}

#[test]
fn rewraps_crlf_and_wide_lines_to_canonical_pem() -> TestResult {
    // Given: two certificates PEM-encoded with CRLF endings and 76-column wrapping.
    let der_one = [0x41_u8; 100];
    let der_two = [0x42_u8; 10];
    let mut pem = String::new();
    for der in [&der_one[..], &der_two[..]] {
        pem.push_str("-----BEGIN CERTIFICATE-----\r\n");
        let encoded = STANDARD.encode(der);
        for chunk_start in (0..encoded.len()).step_by(76) {
            let chunk_end = (chunk_start + 76).min(encoded.len());
            pem.push_str(&encoded[chunk_start..chunk_end]);
            pem.push_str("\r\n");
        }
        pem.push_str("-----END CERTIFICATE-----\r\n");
    }
    let runtime_data = runtime_data();
    let gateway = gateway_quote(&runtime_data);
    let evidence = vec![model_evidence_with_certificate(
        "HOPPER",
        &gpu_nonce(),
        &STANDARD.encode(pem),
    )];

    // When: the model request is built.
    let request = model_request(&gateway, &evidence)?;

    // Then: both certificates survive, in order, re-wrapped at 64 columns with LF.
    let value = serde_json::to_value(request)?;
    assert_eq!(
        value["nvgpu"]["evidence_list"][0]["certificate"],
        canonical_pem_chain_b64(&[&der_one, &der_two])
    );
    Ok(())
}

#[test]
fn fails_closed_on_cert_chain_without_pem_blocks() {
    // Given: a base64 certificate value that decodes to no PEM block at all.
    let runtime_data = runtime_data();
    let gateway = gateway_quote(&runtime_data);
    let evidence = vec![model_evidence_with_certificate(
        "HOPPER",
        &gpu_nonce(),
        "Y2VydA==",
    )];

    // When: the model mapper normalizes the certificate chain.
    let error = model_request(&gateway, &evidence).expect_err("non-PEM chain must fail");

    // Then: the request fails closed instead of forwarding unverifiable bytes to ITA.
    assert!(matches!(
        error,
        ItaEvidenceError::InvalidPemChain {
            field: "nvgpu.evidence_list.certificate"
        }
    ));
}

#[test]
fn fails_closed_on_unterminated_pem_block() {
    // Given: a chain whose BEGIN marker has no matching END marker.
    let runtime_data = runtime_data();
    let gateway = gateway_quote(&runtime_data);
    let evidence = vec![model_evidence_with_certificate(
        "HOPPER",
        &gpu_nonce(),
        &STANDARD.encode("-----BEGIN CERTIFICATE-----\nAAAA\n"),
    )];

    // When: the model mapper normalizes the certificate chain.
    let error = model_request(&gateway, &evidence).expect_err("unterminated block must fail");

    // Then: the truncated chain is rejected.
    assert!(matches!(error, ItaEvidenceError::InvalidPemChain { .. }));
}

#[test]
fn fails_closed_on_invalid_base64_inside_pem_block() {
    // Given: a PEM block whose payload is not valid base64.
    let runtime_data = runtime_data();
    let gateway = gateway_quote(&runtime_data);
    let evidence = vec![model_evidence_with_certificate(
        "HOPPER",
        &gpu_nonce(),
        &STANDARD.encode("-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n"),
    )];

    // When: the model mapper normalizes the certificate chain.
    let error = model_request(&gateway, &evidence).expect_err("bad payload must fail");

    // Then: the invalid payload is reported against the certificate field.
    assert!(matches!(
        error,
        ItaEvidenceError::InvalidBase64 {
            field: "nvgpu.evidence_list.certificate",
            ..
        }
    ));
}
