//! TypeSafe-compatible typed decisions, with the same billing and trust policy
//! for external and self-hosted providers.
use crate::{
    middleware::{auth::AuthenticatedApiKey, RequestBodyHash},
    models::ErrorResponse,
    routes::{api::AppState, common, completions, extractors::OpenAiJson},
};
use axum::{
    body::Body,
    extract::{Extension, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use inference_providers::{CompletionError, SystemOneRequest, SystemOneResponse};
use services::{
    attestation::SignatureKind,
    completions::hash_inference_id_to_uuid,
    models::ModelsError,
    usage::{InferenceType, RecordUsageServiceRequest, ServedProviderTier, StopReason},
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const HEADER_SIGNATURE_ID: &str = "x-signature-id";

#[utoipa::path(
    post,
    path = "/v1/systemone",
    tag = "Decisions",
    request_body = SystemOneRequest,
    responses(
        (status = 200, description = "Typed decisions. X-Signature-Id identifies the receipt at /v1/signature/{id}; Inference-Id identifies billing usage.", body = SystemOneResponse),
        (status = 400, description = "Invalid request or model modality", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 429, description = "Rate or concurrency limit", body = ErrorResponse),
        (status = 502, description = "Invalid provider response", body = ErrorResponse)
    ),
    security(("ApiKeyAuth" = []))
)]
pub async fn systemone(
    State(state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    headers: HeaderMap,
    OpenAiJson(mut request): OpenAiJson<SystemOneRequest>,
) -> Response {
    if let Err(message) = request.validate() {
        return error(StatusCode::BAD_REQUEST, message);
    }
    let encryption = match common::validate_encryption_headers(&headers) {
        Ok(value) => value,
        Err(response) => return response.into_response(),
    };
    // Encrypted state/questions and key-pinned routing need a separate protocol
    // contract. Never silently send an encrypted request to a plaintext fallback.
    if encryption.signing_algo.is_some()
        || encryption.client_pub_key.is_some()
        || encryption.model_pub_key.is_some()
        || encryption.encryption_version.is_some()
        || encryption.encrypt_all_fields.is_some()
    {
        return error(
            StatusCode::BAD_REQUEST,
            "Encryption headers are unavailable for System One",
        );
    }
    let model = match state
        .models_service
        .resolve_and_get_model(&request.model)
        .await
    {
        Ok(model) => model,
        Err(ModelsError::NotFound(_) | ModelsError::InvalidParams(_)) => {
            return error(StatusCode::BAD_REQUEST, "Unknown or inactive model");
        }
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to resolve model"),
    };
    if let Err(message) = model.validate_endpoint(services::models::InferenceEndpoint::SystemOne) {
        return error(StatusCode::BAD_REQUEST, message);
    }
    let alias = if request.model != model.model_name {
        if common::no_aliasing_requested(&headers) {
            return error(
                StatusCode::BAD_REQUEST,
                "Model alias resolution is disabled for this request",
            );
        }
        // The warning belongs in a header: changing the body would break signatures.
        match HeaderValue::from_str(&format!("{} -> {}", request.model, model.model_name)) {
            Ok(value) => Some(value),
            Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid model alias"),
        }
    } else {
        None
    };
    request.model = model.model_name.clone();
    let api_key_id = match Uuid::parse_str(&api_key.api_key.id.0) {
        Ok(id) => id,
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Invalid billing context"),
    };
    let slot = match state
        .completion_service
        .acquire_concurrent_slot(api_key.organization.id.0, model.id, &model.model_name)
        .await
    {
        Ok(slot) => slot,
        Err(services::completions::CompletionError::RateLimitExceeded(message)) => {
            return error(StatusCode::TOO_MANY_REQUESTS, &message);
        }
        Err(err) => {
            return error(
                common::map_domain_error_to_status(&err),
                "Failed to acquire concurrency slot",
            )
        }
    };
    let served = match state
        .inference_provider_pool
        .systemone_with_attribution(
            request,
            body_hash.hash.clone(),
            !api_key.organization.fallback_enabled(),
        )
        .await
    {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!(model = %model.model_name, error = %err, "System One inference failed");
            return provider_error(err);
        }
    };
    // Once inference succeeds, finalize billing and signature pins even if the
    // client disconnects while waiting for persistence.
    let finalize = tokio::spawn(async move {
        let _slot = slot;
        let id = &served.signature_id;
        let inference_id = hash_inference_id_to_uuid(id);
        let usage = &served.response.response.usage;
        let usage_request = RecordUsageServiceRequest {
            organization_id: api_key.organization.id.0,
            workspace_id: api_key.workspace.id.0,
            api_key_id,
            model_id: model.id,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: 0,
            cache_write: None,
            profiled_cache_write_tokens: 0,
            requested_service_tier: None,
            provider_service_tier: None,
            inference_type: InferenceType::Decisions,
            ttft_ms: None,
            avg_itl_ms: None,
            inference_id: Some(inference_id),
            provider_request_id: Some(id.clone()),
            stop_reason: Some(StopReason::Completed),
            response_id: None,
            image_count: None,
            provider_attribution: served.provider_attribution,
            discount: None,
        };
        let signature = async {
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                match served.signature_kind {
                    SignatureKind::ProviderTee => {
                        state
                            .attestation_service
                            .store_stream_chat_signature_from_provider(id)
                            .await
                    }
                    SignatureKind::Gateway => {
                        state
                            .attestation_service
                            .store_chat_signature_and_unpin(
                                id,
                                body_hash.hash,
                                hex::encode(Sha256::digest(&served.response.raw_bytes)),
                            )
                            .await
                    }
                }
            })
            .await;
            // The provider helper bounds fetching, but its database write can
            // stall. Always release the affinity pin even if the outer budget
            // cancels that write or the gateway helper's own cleanup.
            state
                .attestation_service
                .release_chat_signature_pin(id)
                .await;
            if !matches!(result, Ok(Ok(()))) {
                tracing::error!(signature_id = %id, "System One signature finalization failed or timed out");
            }
        };
        tokio::join!(
            signature,
            completions::record_usage_with_sync_fallback(
                state.usage_service,
                usage_request,
                "System One"
            ),
        );
        let tier = completions::provider_tier_to_str(
            match served.provider_attribution.served_provider_tier {
                Some(ServedProviderTier::Near) => inference_providers::ProviderTier::Near,
                Some(ServedProviderTier::Attested3p) => {
                    inference_providers::ProviderTier::Attested3p
                }
                _ => inference_providers::ProviderTier::NonAttested,
            },
        );
        let mut response = Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .header(HEADER_SIGNATURE_ID, id)
            .header(completions::HEADER_INFERENCE_ID, inference_id.to_string())
            .header("x-serving-provider", tier)
            .header(
                header::ACCESS_CONTROL_EXPOSE_HEADERS,
                "X-Signature-Id, Inference-Id, X-Serving-Provider, X-Model-Alias-Resolved",
            );
        if let Some(alias) = alias {
            response = response.header(common::HEADER_MODEL_ALIAS_RESOLVED, alias);
        }
        response
            .body(Body::from(served.response.raw_bytes))
            .expect("validated System One response headers")
    });
    finalize.await.unwrap_or_else(|_| {
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to finalize inference",
        )
    })
}

fn error(status: StatusCode, message: &str) -> Response {
    let kind = if status == StatusCode::TOO_MANY_REQUESTS {
        "rate_limit_exceeded"
    } else if status.is_client_error() {
        "invalid_request_error"
    } else {
        "server_error"
    };
    (
        status,
        Json(ErrorResponse::new(message.to_owned(), kind.to_owned())),
    )
        .into_response()
}

fn provider_error(err: CompletionError) -> Response {
    if matches!(
        err,
        CompletionError::HttpError {
            status_code: 429,
            ..
        }
    ) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse::new(
                "System One provider is rate limited".into(),
                "upstream_rate_limit_exceeded".into(),
            )),
        )
            .into_response();
    }
    let status = match err {
        CompletionError::HttpError {
            status_code: 401 | 403,
            ..
        } => StatusCode::BAD_GATEWAY,
        CompletionError::HttpError { status_code, .. } => StatusCode::from_u16(status_code)
            .ok()
            .filter(|status| status.is_client_error() || status.is_server_error())
            .unwrap_or(StatusCode::BAD_GATEWAY),
        CompletionError::Timeout { .. } => StatusCode::GATEWAY_TIMEOUT,
        _ => StatusCode::BAD_GATEWAY,
    };
    error(status, "System One provider could not complete the request")
}

#[cfg(test)]
mod tests {
    use utoipa::OpenApi;

    #[tokio::test]
    async fn systemone_upstream_rate_limit_is_distinct_from_gateway_limit() {
        let response = super::provider_error(inference_providers::CompletionError::HttpError {
            status_code: 429,
            message: "private upstream body".into(),
            is_external: true,
        });
        assert_eq!(response.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"]["type"], "upstream_rate_limit_exceeded");
        assert!(!value.to_string().contains("private upstream body"));
    }

    #[test]
    fn systemone_openapi_keeps_usage_schemas_distinct() {
        let spec = serde_json::to_value(crate::openapi::ApiDoc::openapi()).unwrap();
        assert_eq!(
            spec["paths"]["/v1/systemone"]["post"]["requestBody"]["content"]["application/json"]
                ["schema"]["$ref"],
            "#/components/schemas/SystemOneRequest",
        );
        let schemas = &spec["components"]["schemas"];
        assert!(schemas["Usage"]["properties"].get("total_tokens").is_some());
        assert!(schemas["SystemOneUsage"]["properties"]
            .get("total_tokens")
            .is_none());
        assert_eq!(
            schemas["SystemOneResponse"]["properties"]["usage"]["$ref"],
            "#/components/schemas/SystemOneUsage",
        );
    }
}
