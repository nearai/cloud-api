use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::Value;
use sha2::{Digest, Sha512};

use super::{
    client::client_test_support::{FakeIta, FakeStep},
    ItaGatewaySigningAlg, ItaTokenQuery,
};
use crate::attestation::{ports::AttestationServiceTrait, AttestationError};

#[path = "service_test_support.rs"]
mod service_test_support;
use service_test_support::{
    ita_config, service_for_fake_ita, service_for_fake_ita_with_timeout, service_with_ita_client,
    RecordingGatewayQuoteCollector, RecordingModelCollector,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
async fn service_unavailable_when_ita_disabled_or_missing_config() -> TestResult {
    // Given: ITA is disabled on one service and enabled without a client on another.
    let disabled = service_with_ita_client(ita_config(false, "https://ita.example.test")?, None);
    let missing_client =
        service_with_ita_client(ita_config(true, "https://ita.example.test")?, None);

    // When: callers request an ITA attestation token.
    let disabled_error = disabled
        .get_ita_attestation_token(query(None))
        .await
        .expect_err("disabled ITA must fail");
    let missing_error = missing_client
        .get_ita_attestation_token(query(None))
        .await
        .expect_err("missing ITA client must fail");

    // Then: both errors are service-unavailable style variants for Task 5 mapping.
    assert!(matches!(
        disabled_error,
        AttestationError::ItaUnavailable { .. }
    ));
    assert!(matches!(
        missing_error,
        AttestationError::ItaUnavailable { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn gateway_flow_gets_nonce_collects_bound_quote_then_attests() -> TestResult {
    // Given: a fake ITA upstream and a quote collector that records report_data bytes.
    let server = FakeIta::start(vec![
        FakeStep::json(
            200,
            r#"{"val":"dmVyaWZpZXItdmFsdWU=","iat":"aWF0LWJ5dGVz","signature":"dmVyaWZpZXItc2lnbmF0dXJl"}"#,
        ),
        FakeStep::json(200, r#"{"token":"gateway.jwt","request_id":"attest-1"}"#),
    ])
    .await?;
    let gateway_collector = RecordingGatewayQuoteCollector::default();
    let service = service_for_fake_ita(&server, 0, gateway_collector.clone(), None)?;

    // When: no model is requested.
    let response = service.get_ita_attestation_token(query(None)).await?;

    // Then: the service returns a gateway token and no model tokens.
    assert_eq!(response.gateway.token, "gateway.jwt");
    assert_eq!(response.gateway.ita_request_id.as_deref(), Some("attest-1"));
    assert!(response.models.is_empty());

    let ita_requests = server.requests();
    assert_eq!(ita_requests.len(), 2);
    assert_eq!(ita_requests[0].path, "/appraisal/v2/nonce");
    assert_eq!(ita_requests[1].path, "/appraisal/v2/attest");

    let attest_body: Value = serde_json::from_slice(&ita_requests[1].body)?;
    let runtime_data = STANDARD.decode(
        attest_body["tdx"]["runtime_data"]
            .as_str()
            .ok_or("missing runtime data")?,
    )?;
    let expected_report_data = expected_ita_report_data(&runtime_data);
    let calls = gateway_collector.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].report_data, expected_report_data);
    Ok(())
}

#[tokio::test]
async fn model_flow_uses_canonical_model_and_returns_alias_metadata() -> TestResult {
    // Given: a model alias resolves to a canonical model with ITA-compatible provider evidence.
    let server = FakeIta::start(vec![
        FakeStep::json(
            200,
            r#"{"val":"dmVyaWZpZXItdmFsdWU=","iat":"aWF0LWJ5dGVz","signature":"dmVyaWZpZXItc2lnbmF0dXJl"}"#,
        ),
        FakeStep::json(200, r#"{"token":"gateway.jwt"}"#),
        FakeStep::json(200, r#"{"token":"model.jwt","request_id":"model-attest"}"#),
    ])
    .await?;
    let model_collector = RecordingModelCollector::compatible();
    let service = service_for_fake_ita(
        &server,
        0,
        RecordingGatewayQuoteCollector::default(),
        Some(model_collector.clone()),
    )?;

    // When: the request names the alias.
    let response = service
        .get_ita_attestation_token(query(Some("alias-model")))
        .await?;

    // Then: provider evidence is requested for the canonical name and alias metadata is preserved.
    assert_eq!(response.models.len(), 1);
    assert_eq!(response.models[0].model, "canonical-model");
    assert_eq!(response.models[0].attestation.token, "model.jwt");
    let alias = response
        .model_alias_resolved
        .as_ref()
        .ok_or("missing alias metadata")?;
    assert_eq!(alias.requested, "alias-model");
    assert_eq!(alias.canonical, "canonical-model");
    assert_eq!(model_collector.calls(), vec!["canonical-model"]);
    Ok(())
}

#[tokio::test]
async fn model_flow_fails_closed_on_incompatible_provider_evidence() -> TestResult {
    // Given: a model provider returns evidence without ITA-compatible GPU nonce binding.
    let server = FakeIta::start(vec![FakeStep::json(
        200,
        r#"{"val":"dmVyaWZpZXItdmFsdWU=","iat":"aWF0LWJ5dGVz","signature":"dmVyaWZpZXItc2lnbmF0dXJl"}"#,
    )])
    .await?;
    let model_collector = RecordingModelCollector::incompatible();
    let service = service_for_fake_ita(
        &server,
        0,
        RecordingGatewayQuoteCollector::default(),
        Some(model_collector),
    )?;

    // When: model evidence cannot be appraised.
    let error = service
        .get_ita_attestation_token(query(Some("alias-model")))
        .await
        .expect_err("incompatible evidence must fail");

    // Then: the service returns an explicit evidence/provider error and does not attest misleading tokens.
    assert!(matches!(error, AttestationError::ItaInvalidEvidence { .. }));
    assert_eq!(server.requests().len(), 1);
    Ok(())
}

#[tokio::test]
async fn ita_rate_limit_preserves_retry_after() -> TestResult {
    // Given: ITA returns a nonce, then rate-limits token appraisal.
    let server = FakeIta::start(vec![
        FakeStep::json(
            200,
            r#"{"val":"dmVyaWZpZXItdmFsdWU=","iat":"aWF0LWJ5dGVz","signature":"dmVyaWZpZXItc2lnbmF0dXJl"}"#,
        ),
        FakeStep::json(429, r#"{}"#).with_header("Retry-After", "2"),
    ])
    .await?;
    let service =
        service_for_fake_ita(&server, 0, RecordingGatewayQuoteCollector::default(), None)?;

    // When: token appraisal is rate-limited.
    let error = service
        .get_ita_attestation_token(query(None))
        .await
        .expect_err("rate limit must fail");

    // Then: Retry-After is retained for HTTP mapping.
    assert!(matches!(
        error,
        AttestationError::ItaRateLimited {
            retry_after: Some(value)
        } if value == "2"
    ));
    Ok(())
}

#[tokio::test]
async fn ita_timeout_maps_to_timeout_error() -> TestResult {
    // Given: ITA stalls on the verifier nonce request.
    let server = FakeIta::start(vec![FakeStep::Hang]).await?;
    let service = service_for_fake_ita_with_timeout(
        &server,
        0,
        25,
        RecordingGatewayQuoteCollector::default(),
        None,
    )?;

    // When: the ITA client times out.
    let error = service
        .get_ita_attestation_token(query(None))
        .await
        .expect_err("timeout must fail");

    // Then: the service exposes a typed timeout error for Task 5.
    assert!(matches!(error, AttestationError::ItaTimeout));
    Ok(())
}

fn query(model: Option<&str>) -> ItaTokenQuery {
    ItaTokenQuery {
        model: model.map(str::to_string),
        nonce: Some("00".repeat(32)),
        signing_algo: Some(ItaGatewaySigningAlg::Ed25519),
        signing_address: None,
        include_tls_fingerprint: Some(false),
        policy_ids: None,
        policy_must_match: None,
        token_signing_alg: None,
    }
}

fn expected_ita_report_data(runtime_data: &[u8]) -> Vec<u8> {
    let mut hasher = Sha512::new();
    hasher.update(b"verifier-valueiat-bytes");
    hasher.update(runtime_data);
    hasher.finalize().to_vec()
}
