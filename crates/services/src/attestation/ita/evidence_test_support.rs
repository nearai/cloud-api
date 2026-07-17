use config::{ItaEffectivePolicy, ItaPolicyIds, ItaTokenSigningAlg};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256, Sha512};

use super::*;
use crate::attestation::ita::ItaVerifierNonce;
use crate::attestation::models::DstackCpuQuote;

pub(super) type TestResult = Result<(), Box<dyn std::error::Error>>;

pub(super) const POLICY_A: &str = "11111111-1111-4111-8111-111111111111";
pub(super) const POLICY_B: &str = "22222222-2222-4222-8222-222222222222";
pub(super) const DSTACK_EVENT_LOG: &str =
    r#"[{"imr":3,"event_type":1,"digest":"00","event":"test","event_payload":"00"}]"#;

pub(super) fn verifier_nonce() -> ItaVerifierNonce {
    ItaVerifierNonce {
        val: "dmVyaWZpZXItdmFsdWU=".to_string(),
        iat: "aWF0LWJ5dGVz".to_string(),
        signature: "dmVyaWZpZXItc2lnbmF0dXJl".to_string(),
    }
}

fn nonce_material() -> Vec<u8> {
    b"verifier-valueiat-bytes".to_vec()
}

pub(super) fn effective_policy(policy_ids: &str) -> Result<ItaEffectivePolicy, std::io::Error> {
    Ok(ItaEffectivePolicy {
        policy_ids: ItaPolicyIds::parse_csv(policy_ids, "policy_ids")
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?,
        policy_must_match: true,
        token_signing_alg: ItaTokenSigningAlg::Rs256,
    })
}

fn empty_policy() -> ItaEffectivePolicy {
    ItaEffectivePolicy {
        policy_ids: ItaPolicyIds::default(),
        policy_must_match: true,
        token_signing_alg: ItaTokenSigningAlg::Rs256,
    }
}

pub(super) fn runtime_data() -> Vec<u8> {
    br#"{"signing_algo":"ed25519","signing_address":"0xabc123","caller_nonce":"0000000000000000000000000000000000000000000000000000000000000000","tls_cert_fingerprint":"1111111111111111111111111111111111111111111111111111111111111111"}"#.to_vec()
}

fn report_data(runtime_data: &[u8]) -> String {
    let mut hasher = Sha512::new();
    hasher.update(nonce_material());
    hasher.update(runtime_data);
    hex::encode(hasher.finalize())
}

pub(super) fn gateway_quote(runtime_data: &[u8]) -> DstackCpuQuote {
    DstackCpuQuote {
        signing_address: "0xabc123".to_string(),
        signing_algo: "ed25519".to_string(),
        intel_quote: "0x01020304".to_string(),
        event_log: DSTACK_EVENT_LOG.to_string(),
        report_data: report_data(runtime_data),
        request_nonce: "00".repeat(32),
        info: json!({}),
        vpc: None,
        tls_cert_fingerprint: Some("11".repeat(32)),
    }
}

pub(super) fn gpu_nonce() -> String {
    let mut hasher = Sha256::new();
    hasher.update(nonce_material());
    hex::encode(hasher.finalize())
}

pub(super) fn model_evidence(arch: &str, gpu_nonce: &str) -> Map<String, Value> {
    let payload = json!({
        "gpu_nonce": gpu_nonce,
        "arch": arch,
        "evidence_list": [{ "certificate": "Y2VydA==", "evidence": "ZXZpZGVuY2U=" }]
    });
    let mut evidence = Map::new();
    evidence.insert(
        "nvidia_payload".to_string(),
        Value::String(payload.to_string()),
    );
    evidence
}

pub(super) fn gateway_request(
    gateway: &DstackCpuQuote,
) -> Result<ItaAttestRequest, ItaEvidenceError> {
    build_gateway_attest_request(ItaGatewayEvidenceInput {
        gateway,
        verifier_nonce: &verifier_nonce(),
        policy: empty_policy(),
    })
}

pub(super) fn model_request(
    gateway: &DstackCpuQuote,
    model_attestations: &[Map<String, Value>],
) -> Result<ItaAttestRequest, ItaEvidenceError> {
    build_model_attest_request(ItaModelEvidenceInput {
        gateway,
        model_attestations,
        verifier_nonce: &verifier_nonce(),
        policy: empty_policy(),
    })
}
