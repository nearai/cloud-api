//! Session-authorized, audience-bound admission credentials and their public keys.
use crate::middleware::{auth_middleware, AuthState, AuthenticatedUser};
use crate::models::ErrorResponse;
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Extension, Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use services::auth::{
    admission::{
        AdmissionError, AdmissionIdentity, AdmissionIssuer, AdmissionIssuerConfig, AdmissionRequest,
    },
    UserId,
};
use std::sync::Arc;

pub type AdmissionState = Option<Arc<AdmissionIssuer>>;

pub fn configured_issuer(
    config: Option<&config::AdmissionProofConfig>,
) -> Result<AdmissionState, AdmissionError> {
    let Some(config) = config else {
        return Ok(None);
    };
    let bytes = |value: &str| -> Result<[u8; 32], AdmissionError> {
        if value.len() != 43 {
            return Err(AdmissionError::InvalidConfiguration);
        }
        let decoded: [u8; 32] = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| AdmissionError::InvalidConfiguration)?
            .try_into()
            .map_err(|_| AdmissionError::InvalidConfiguration)?;
        if URL_SAFE_NO_PAD.encode(decoded) != value {
            return Err(AdmissionError::InvalidConfiguration);
        }
        Ok(decoded)
    };
    let settings = AdmissionIssuerConfig::new(
        config.issuer.clone(),
        config.audiences.clone(),
        bytes(&config.subject_key)?,
        bytes(&config.signing_seed)?,
        config
            .retained_public_keys
            .iter()
            .map(|key| bytes(key))
            .collect::<Result<Vec<_>, _>>()?,
    )?;
    Ok(Some(Arc::new(AdmissionIssuer::new(settings))))
}

fn private_response(response: impl IntoResponse) -> Response {
    let mut response = response.into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

/// Admission always requires a verified session ID, even while ordinary Cloud
/// routes still accept legacy access tokens during their compatibility window.
pub async fn session_bound_auth(
    State(state): State<AuthState>,
    request: Request,
    next: Next,
) -> Response {
    let claims = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(|token| {
            state
                .auth_service
                .validate_session_access_token(token.to_owned(), state.encoding_key.clone())
                .ok()
                .flatten()
        });
    if claims.and_then(|claims| claims.sid).is_none() {
        return private_response((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse::new(
                "Invalid or expired access token".into(),
                "unauthorized".into(),
            )),
        ));
    }
    // The established middleware checks this session's owner, expiry and live
    // database presence, plus global user revocation, before inserting the user.
    match auth_middleware(State(state), request, next).await {
        Ok(response) => private_response(response),
        Err(error) => private_response(error),
    }
}

#[utoipa::path(
    post, path = "/v1/auth/admission-proof", tag = "Authentication",
    request_body = services::auth::admission::AdmissionRequest,
    responses((status = 200, description = "Short-lived admission assertion; grants no Cloud API access", body = services::auth::admission::AdmissionAssertion),
        (status = 400, description = "Invalid challenge or audience"),
        (status = 401, description = "Invalid session"),
        (status = 403, description = "Ineligible user"),
        (status = 503, description = "Admission proofs are disabled")),
    security(("session_token" = []))
)]
pub async fn issue_proof(
    State(issuer): State<AdmissionState>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(request): Json<AdmissionRequest>,
) -> Response {
    let Some(issuer) = issuer else {
        return private_response(StatusCode::SERVICE_UNAVAILABLE);
    };
    let identity = AdmissionIdentity {
        user_id: UserId(user.0.id),
        auth_provider: &user.0.auth_provider,
        is_active: user.0.is_active,
    };
    match issuer.issue(&identity, &request, chrono::Utc::now().timestamp()) {
        Ok(assertion) => private_response(Json(assertion)),
        Err(error) => {
            let status = match error {
                AdmissionError::IneligibleUser => StatusCode::FORBIDDEN,
                AdmissionError::InvalidAudience
                | AdmissionError::InvalidNonce
                | AdmissionError::InvalidDevicePublicKey => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            // Errors carry only fixed labels, never signed claims or credentials.
            private_response((
                status,
                Json(crate::models::ErrorResponse::new(
                    error.to_string(),
                    "admission_proof_rejected".into(),
                )),
            ))
        }
    }
}

#[utoipa::path(get, path = "/v1/auth/admission-proof/jwks", tag = "Authentication",
    responses((status = 200, description = "Ed25519 assertion verification keys", body = services::auth::admission::AdmissionJwks),
        (status = 503, description = "Admission proofs are disabled")), security(()))]
pub async fn public_keys(State(issuer): State<AdmissionState>) -> Response {
    match issuer {
        Some(issuer) => (
            [(header::CACHE_CONTROL, "public, max-age=30")],
            Json(issuer.jwks().clone()),
        )
            .into_response(),
        None => private_response(StatusCode::SERVICE_UNAVAILABLE),
    }
}

#[cfg(test)]
mod tests {
    use crate::routes::admission::configured_issuer;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

    #[test]
    fn admission_openapi_publishes_required_typed_contracts() {
        let spec =
            serde_json::to_value(<crate::openapi::ApiDoc as utoipa::OpenApi>::openapi()).unwrap();
        let schemas = &spec["components"]["schemas"];
        let required = schemas["AdmissionRequest"]["required"]
            .as_array()
            .expect("typed request schema");
        for field in ["audience", "nonce", "device_public_key"] {
            assert!(required.contains(&serde_json::json!(field)));
        }
        assert_eq!(
            schemas["AdmissionAssertion"]["properties"]["assertion"]["type"],
            "string"
        );
        assert_eq!(
            schemas["AdmissionAssertion"]["properties"]["expires_at"]["type"],
            "integer"
        );
        assert_eq!(
            schemas["AdmissionJwks"]["properties"]["keys"]["type"],
            "array"
        );
        assert!(schemas["AdmissionJwk"]["properties"].get("d").is_none());
        assert_eq!(
            spec["paths"]["/v1/auth/admission-proof/jwks"]["get"]["security"],
            serde_json::json!([{}])
        );
        assert_eq!(
            spec["paths"]["/v1/auth/admission-proof"]["post"]["requestBody"]["content"]
                ["application/json"]["schema"]["$ref"],
            "#/components/schemas/AdmissionRequest"
        );
    }

    #[test]
    fn disabled_and_malformed_configuration_are_distinct() {
        assert!(configured_issuer(None).unwrap().is_none());
        let mut config = config::AdmissionProofConfig {
            issuer: "https://cloud-api.near.ai".into(),
            audiences: vec!["trace-commons".into()],
            subject_key: URL_SAFE_NO_PAD.encode([1; 32]),
            signing_seed: URL_SAFE_NO_PAD.encode([2; 32]),
            retained_public_keys: Vec::new(),
        };
        assert!(configured_issuer(Some(&config)).unwrap().is_some());
        config.signing_seed = "bad-input-do-not-log".into();
        let error = configured_issuer(Some(&config)).err().unwrap();
        assert!(!error.to_string().contains("bad-input-do-not-log"));
    }
}
