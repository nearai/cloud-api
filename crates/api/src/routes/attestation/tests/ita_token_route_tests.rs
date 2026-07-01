use super::{
    super::ItaTokenResponse,
    ita_token_route_test_support::{
        public_ita_server, sample_ita_response, RecordingItaAttestationService, TestModelsService,
    },
};
use crate::models::ErrorResponse;
use crate::routes::common::HEADER_MODEL_ALIAS_RESOLVED;
use axum::http::StatusCode;
use config::{ItaPolicyIds, ItaTokenSigningAlg};
use services::attestation::{
    ita::{ItaGatewaySigningAlg, ItaModelAliasResolved as ServiceItaModelAliasResolved},
    AttestationError,
};

const POLICY_A: &str = "11111111-1111-4111-8111-111111111111";
const POLICY_B: &str = "22222222-2222-4222-8222-222222222222";

#[tokio::test]
async fn ita_token_route_accepts_valid_public_query_without_auth() {
    let attestation = RecordingItaAttestationService::ok(sample_ita_response(None));
    let server = public_ita_server(attestation.clone(), TestModelsService::default());

    let response = server
        .get("/attestation/ita-token")
        .add_query_param(
            "nonce",
            "0000000000000000000000000000000000000000000000000000000000000001",
        )
        .add_query_param("signing_algo", "ed25519")
        .add_query_param("signing_address", "near1signer")
        .add_query_param("include_tls_fingerprint", "true")
        .add_query_param("policy_ids", &format!("{POLICY_A},{POLICY_B}"))
        .add_query_param("policy_must_match", "true")
        .add_query_param("token_signing_alg", "RS256")
        .await;

    assert_eq!(response.status_code(), StatusCode::OK);
    let body = response.json::<ItaTokenResponse>();
    assert_eq!(body.gateway.token, "gateway.jwt");
    assert_eq!(body.gateway.token_type, "JWT");
    assert_eq!(body.gateway.attestation_type, "tdx");
    assert_eq!(body.gateway.token_signing_alg, "RS256");
    assert_eq!(
        body.gateway.ita_request_id.as_deref(),
        Some("gateway-request")
    );
    assert!(body.models.is_empty());
    assert_eq!(body.jwks_url, "https://portal.example.test/certs");
    assert_eq!(body.policy_ids, vec![POLICY_A, POLICY_B]);
    assert!(body.policy_must_match);
    assert_eq!(
        body.nonce,
        "0000000000000000000000000000000000000000000000000000000000000001"
    );

    let query = attestation.only_query();
    assert_eq!(
        query.nonce.as_deref(),
        Some("0000000000000000000000000000000000000000000000000000000000000001")
    );
    assert_eq!(query.signing_algo, Some(ItaGatewaySigningAlg::Ed25519));
    assert_eq!(query.signing_address.as_deref(), Some("near1signer"));
    assert_eq!(query.include_tls_fingerprint, Some(true));
    assert_eq!(
        query.policy_ids.as_ref().map(ItaPolicyIds::to_strings),
        Some(vec![POLICY_A.to_string(), POLICY_B.to_string()])
    );
    assert_eq!(query.policy_must_match, Some(true));
    assert_eq!(query.token_signing_alg, Some(ItaTokenSigningAlg::Rs256));
}

#[tokio::test]
async fn ita_token_route_rejects_invalid_query_params_with_param_names() {
    for (raw_query, expected_param) in [
        ("nonce=abc", "nonce"),
        ("policy_ids=bad%20id", "policy_ids"),
        ("policy_ids=policy-a", "policy_ids"),
        ("policy_must_match=maybe", "policy_must_match"),
        ("token_signing_alg=PS256", "token_signing_alg"),
    ] {
        let attestation = RecordingItaAttestationService::ok(sample_ita_response(None));
        let server = public_ita_server(attestation.clone(), TestModelsService::default());

        let response = server
            .get(&format!("/attestation/ita-token?{raw_query}"))
            .await;

        assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
        let body = response.json::<ErrorResponse>();
        assert_eq!(body.error.r#type, "invalid_request_error");
        assert_eq!(body.error.param.as_deref(), Some(expected_param));
        assert!(attestation.queries().is_empty());
    }
}

#[tokio::test]
async fn ita_token_route_announces_model_alias_header() {
    let attestation = RecordingItaAttestationService::ok(sample_ita_response(Some(
        ServiceItaModelAliasResolved {
            requested: "alias-model".to_string(),
            canonical: "canonical-model".to_string(),
        },
    )));
    let server = public_ita_server(
        attestation.clone(),
        TestModelsService::with_alias("alias-model", "canonical-model"),
    );

    let response = server
        .get("/attestation/ita-token")
        .add_query_param("model", "alias-model")
        .await;

    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(
        response.header(HEADER_MODEL_ALIAS_RESOLVED),
        "alias-model -> canonical-model"
    );
    let body = response.json::<ItaTokenResponse>();
    let alias = body
        .model_alias_resolved
        .unwrap_or_else(|| panic!("alias metadata should be serialized"));
    assert_eq!(alias.requested, "alias-model");
    assert_eq!(alias.canonical, "canonical-model");
    assert_eq!(
        attestation.only_query().model.as_deref(),
        Some("alias-model")
    );
}

#[tokio::test]
async fn ita_token_route_rejects_alias_when_no_aliasing_requested() {
    let attestation = RecordingItaAttestationService::ok(sample_ita_response(None));
    let server = public_ita_server(
        attestation.clone(),
        TestModelsService::with_alias("alias-model", "canonical-model"),
    );

    let response = server
        .get("/attestation/ita-token")
        .add_query_param("model", "alias-model")
        .add_header(crate::routes::common::HEADER_NO_ALIASING, "true")
        .await;

    assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
    let body = response.json::<ErrorResponse>();
    assert_eq!(body.error.r#type, "invalid_request_error");
    assert_eq!(body.error.param.as_deref(), Some("model"));
    assert!(attestation.queries().is_empty());
}

#[tokio::test]
async fn ita_token_route_maps_service_errors_to_http_contract() {
    for scenario in [
        ItaErrorScenario {
            error: AttestationError::ItaUnavailable {
                reason: "ITA attestation is disabled".to_string(),
            },
            status: StatusCode::SERVICE_UNAVAILABLE,
            error_type: "service_unavailable",
            retry_after: None,
        },
        ItaErrorScenario {
            error: AttestationError::ItaRateLimited {
                retry_after: Some("2".to_string()),
            },
            status: StatusCode::TOO_MANY_REQUESTS,
            error_type: "rate_limit_error",
            retry_after: Some("2"),
        },
        ItaErrorScenario {
            error: AttestationError::ItaTimeout,
            status: StatusCode::GATEWAY_TIMEOUT,
            error_type: "timeout_error",
            retry_after: None,
        },
    ] {
        let server = public_ita_server(
            RecordingItaAttestationService::err(scenario.error),
            TestModelsService::default(),
        );

        let response = server.get("/attestation/ita-token").await;

        assert_eq!(response.status_code(), scenario.status);
        let body = response.json::<ErrorResponse>();
        assert_eq!(body.error.r#type, scenario.error_type);
        assert_eq!(body.error.param, None);
        match scenario.retry_after {
            Some(expected) => assert_eq!(response.header("retry-after"), expected),
            None => assert!(response.maybe_header("retry-after").is_none()),
        }
    }
}

#[tokio::test]
async fn ita_token_route_maps_bad_upstream_to_502() {
    let server = public_ita_server(
        RecordingItaAttestationService::err(AttestationError::ItaBadUpstream {
            reason: "missing token".to_string(),
        }),
        TestModelsService::default(),
    );

    let response = server.get("/attestation/ita-token").await;

    assert_eq!(response.status_code(), StatusCode::BAD_GATEWAY);
    let body = response.json::<ErrorResponse>();
    assert_eq!(body.error.r#type, "bad_gateway");
    assert_eq!(body.error.param, None);
    assert!(response.maybe_header("retry-after").is_none());
}

#[tokio::test]
async fn ita_token_route_maps_invalid_policy_to_400_with_policy_param() {
    let server = public_ita_server(
        RecordingItaAttestationService::err(AttestationError::InvalidParameter(
            "policy_ids contains an invalid policy id".to_string(),
        )),
        TestModelsService::default(),
    );

    let response = server.get("/attestation/ita-token").await;

    assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
    let body = response.json::<ErrorResponse>();
    assert_eq!(body.error.r#type, "invalid_request_error");
    assert_eq!(body.error.param.as_deref(), Some("policy_ids"));
    assert!(response.maybe_header("retry-after").is_none());
}

struct ItaErrorScenario {
    error: AttestationError,
    status: StatusCode,
    error_type: &'static str,
    retry_after: Option<&'static str>,
}
