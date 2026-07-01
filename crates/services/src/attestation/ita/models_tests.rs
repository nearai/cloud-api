use super::*;
use config::{ItaPolicyIds, ItaTokenSigningAlg};
use serde_json::json;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const POLICY_A: &str = "11111111-1111-4111-8111-111111111111";
const POLICY_B: &str = "22222222-2222-4222-8222-222222222222";
const POLICY_C: &str = "33333333-3333-4333-8333-333333333333";

#[test]
fn ita_query_policy_override_carries_only_request_overrides() -> TestResult {
    // Given: a request query supplies policy override fields.
    let query = ItaTokenQuery {
        model: Some("model-a".to_string()),
        nonce: Some("00".repeat(32)),
        signing_algo: Some(ItaGatewaySigningAlg::Ed25519),
        signing_address: None,
        include_tls_fingerprint: Some(true),
        policy_ids: Some(ItaPolicyIds::parse_csv(POLICY_A, "policy_ids")?),
        policy_must_match: Some(true),
        token_signing_alg: Some(ItaTokenSigningAlg::Rs256),
    };

    // When: policy override values are extracted.
    let policy_override = query.policy_override();

    // Then: only ITA policy controls are carried into effective policy calculation.
    assert_eq!(
        policy_override
            .policy_ids
            .as_ref()
            .map(ItaPolicyIds::to_strings),
        Some(vec![POLICY_A.to_string()])
    );
    assert_eq!(policy_override.policy_must_match, Some(true));
    assert_eq!(
        policy_override.token_signing_alg,
        Some(ItaTokenSigningAlg::Rs256)
    );
    Ok(())
}

#[test]
fn ita_query_policy_ids_deserialize_through_validated_csv_boundary() -> TestResult {
    // Given: a URL query supplies comma-separated policy IDs.
    let raw_query = format!(
        "policy_ids={POLICY_A},{POLICY_B},{POLICY_C}&policy_must_match=true&token_signing_alg=RS256"
    );

    // When: the query DTO is deserialized at the HTTP boundary.
    let query = serde_urlencoded::from_str::<ItaTokenQuery>(&raw_query)?;

    // Then: policy IDs are parsed through the bounded policy ID validator.
    assert_eq!(
        query.policy_ids.as_ref().map(ItaPolicyIds::to_strings),
        Some(vec![
            POLICY_A.to_string(),
            POLICY_B.to_string(),
            POLICY_C.to_string()
        ])
    );
    assert_eq!(query.policy_must_match, Some(true));
    assert_eq!(query.token_signing_alg, Some(ItaTokenSigningAlg::Rs256));
    Ok(())
}

#[test]
fn ita_query_policy_ids_deserialize_rejects_invalid_policy_values() {
    // Given: query policy IDs that violate the locked Task 1 policy contract.
    let too_many = (0..=config::MAX_ITA_POLICY_IDS)
        .map(|idx| format!("00000000-0000-4000-8000-{idx:012}"))
        .collect::<Vec<_>>()
        .join(",");
    let invalid_queries = [
        "policy_ids=bad%20id".to_string(),
        "policy_ids=policy-a".to_string(),
        format!("policy_ids={too_many}"),
        format!("policy_ids={POLICY_A},,{POLICY_B}"),
        format!("policy_ids={POLICY_A},%20,{POLICY_B}"),
        "policy_ids=%20".to_string(),
    ];

    // When/Then: every invalid query is rejected during DTO deserialization.
    for raw_query in invalid_queries {
        assert!(
            serde_urlencoded::from_str::<ItaTokenQuery>(&raw_query).is_err(),
            "query should reject invalid policy_ids: {raw_query}"
        );
    }
}

#[test]
fn ita_response_contract_serializes_wrapper_shape() -> TestResult {
    // Given: a gateway-only ITA token response.
    let response = ItaTokenResponse {
        gateway: ItaAttestationToken {
            token: "header.payload.signature".to_string(),
            token_type: ItaTokenType::Jwt,
            attestation_type: ItaAttestationType::Tdx,
            token_signing_alg: ItaTokenSigningAlg::Ps384,
            ita_request_id: Some("request-1".to_string()),
        },
        models: Vec::new(),
        jwks_url: "https://portal.trustauthority.intel.com/certs".to_string(),
        policy_ids: ItaPolicyIds::parse_csv(POLICY_A, "policy_ids")?,
        policy_must_match: true,
        nonce: "00".repeat(32),
        model_alias_resolved: None,
    };

    // When: the response is serialized for the future API wrapper.
    let value = serde_json::to_value(response)?;

    // Then: token, policy, nonce, and JWKS fields have the locked names and enum values.
    assert_eq!(
        value,
        json!({
            "gateway": {
                "token": "header.payload.signature",
                "token_type": "JWT",
                "attestation_type": "tdx",
                "token_signing_alg": "PS384",
                "ita_request_id": "request-1"
            },
            "models": [],
            "jwks_url": "https://portal.trustauthority.intel.com/certs",
            "policy_ids": [POLICY_A],
            "policy_must_match": true,
            "nonce": "0000000000000000000000000000000000000000000000000000000000000000"
        })
    );
    Ok(())
}

#[test]
fn ita_attest_request_serializes_typed_evidence_without_untyped_json_fields() -> TestResult {
    // Given: a typed ITA attest request with TDX evidence.
    let nonce = ItaVerifierNonce {
        val: "bm9uY2UtdmFsdWU=".to_string(),
        iat: "MjAyNi0wNi0zMFQwMDowMDowMFo=".to_string(),
        signature: "bm9uY2Utc2lnbmF0dXJl".to_string(),
    };
    let request = ItaAttestRequest {
        policy_ids: ItaPolicyIds::parse_csv(&format!("{POLICY_A},{POLICY_B}"), "policy_ids")?,
        token_signing_alg: ItaTokenSigningAlg::Rs256,
        policy_must_match: false,
        tdx: Some(ItaTdxEvidence {
            quote: "base64-quote".to_string(),
            runtime_data: "base64-runtime".to_string(),
            event_log: None,
            verifier_nonce: nonce,
        }),
        nvgpu: None,
    };

    // When: the request is serialized for the ITA appraisal boundary.
    let value = serde_json::to_value(request)?;

    // Then: the boundary shape uses concrete typed fields and omits absent evidence sections.
    assert_eq!(
        value,
        json!({
            "policy_ids": [POLICY_A, POLICY_B],
            "token_signing_alg": "RS256",
            "policy_must_match": false,
            "tdx": {
                "quote": "base64-quote",
                "runtime_data": "base64-runtime",
                "verifier_nonce": {
                    "val": "bm9uY2UtdmFsdWU=",
                    "iat": "MjAyNi0wNi0zMFQwMDowMDowMFo=",
                    "signature": "bm9uY2Utc2lnbmF0dXJl"
                }
            }
        })
    );
    Ok(())
}

#[test]
fn verifier_nonce_material_uses_decoded_val_and_iat_bytes() -> TestResult {
    // Given: ITA wire fields use Go-compatible base64-encoded byte slices.
    let nonce = ItaVerifierNonce {
        val: "dmFsLWJ5dGVz".to_string(),
        iat: "aWF0LWJ5dGVz".to_string(),
        signature: "c2lnLWJ5dGVz".to_string(),
    };

    // When: nonce material is built for TDX/GPU binding.
    let material = nonce.nonce_material()?;

    // Then: only decoded Val || Iat bytes are hashed, while signature remains preserved separately.
    assert_eq!(material, b"val-bytesiat-bytes");
    assert_eq!(nonce.signature, "c2lnLWJ5dGVz");
    Ok(())
}
