use super::*;

/// Text embeddings endpoint (passthrough)
///
/// Proxies embedding requests to the appropriate backend model.
/// Only the `model` field is read from the request for routing; the rest is forwarded as-is.
///
/// These documentation-only types approximate the request and response bodies
/// for OpenAPI schema generation. The runtime handler still accepts a raw
/// `Bytes` body and forwards it transparently to the provider.
#[derive(utoipa::ToSchema, serde::Serialize, serde::Deserialize)]
struct EmbeddingsRequestDoc {
    /// ID of the model to use for generating embeddings.
    model: String,
    /// Input text or tokens to embed. This may be a string, array of strings,
    /// or other JSON-compatible structure, so we document it generically.
    #[serde(default)]
    input: serde_json::Value,
}

#[derive(utoipa::ToSchema, serde::Serialize, serde::Deserialize)]
struct EmbeddingsResponseDoc {
    /// Provider-specific embeddings payload; documented generically.
    #[serde(default)]
    data: serde_json::Value,
}

/// Classify an upstream provider HTTP error into the (status, error_type, message)
/// triple we surface to the client.
///
/// Rules:
/// - 401/403/407 from upstream mean *our* credentials to the backend are wrong
///   (or our backend's auth config is broken). The client did nothing to cause
///   it, so we return 500 with a generic message instead of echoing an auth
///   failure that would be misleading.
/// - 404 → 404 not_found_error (e.g. model not found at this provider).
/// - 429 → 429 rate_limit_error.
/// - Other 4xx → preserve upstream status with `invalid_request_error`. The
///   upstream message is surfaced so the user can see *why* their request was
///   rejected (e.g. "dimensions is not supported for this model").
/// - 5xx (and anything else) → 500 server_error with a generic message. The
///   upstream body may contain stack traces or internal details we don't want
///   to leak.
pub(super) fn classify_provider_error(
    upstream_status: u16,
    upstream_message: String,
) -> (StatusCode, &'static str, String) {
    let generic = || {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "Embeddings request failed. Please try again later.".to_string(),
        )
    };
    match upstream_status {
        401 | 403 | 407 => generic(),
        404 => (StatusCode::NOT_FOUND, "not_found_error", upstream_message),
        429 => (
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            upstream_message,
        ),
        s if (400..=499).contains(&s) => {
            let http_status = StatusCode::from_u16(s).unwrap_or(StatusCode::BAD_REQUEST);
            (http_status, "invalid_request_error", upstream_message)
        }
        _ => generic(),
    }
}

#[utoipa::path(
    post,
    path = "/v1/embeddings",
    tag = "Embeddings",
    request_body = EmbeddingsRequestDoc,
    responses(
        (status = 200, description = "Embeddings generated successfully", body = EmbeddingsResponseDoc),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 429, description = "Rate limited or overloaded — retry with backoff. Check error.type: rate_limit_exceeded or service_overloaded.", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse),
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn embeddings(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(_body_hash): Extension<RequestBodyHash>,
    Extension(correlation): Extension<RequestCorrelation>,
    headers: header::HeaderMap,
    requested_model: RequestedModel,
    body: Bytes,
) -> axum::response::Response {
    // Minimal deserialization: extract only the model name for routing
    #[derive(serde::Deserialize)]
    struct ModelExtract {
        model: String,
    }

    let model_name = match serde_json::from_slice::<ModelExtract>(&body) {
        Ok(extract) => extract.model,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    format!("Invalid request body: {e}"),
                    "invalid_request_error".to_string(),
                )),
            )
                .into_response();
        }
    };
    requested_model.set(&model_name);

    debug!(
        "Embeddings request: model={}, org={}, workspace={}",
        model_name, api_key.organization.id, api_key.workspace.id.0
    );

    // Resolve model to get UUID for usage tracking
    let model = match app_state
        .models_service
        .get_model_by_name(&model_name)
        .await
    {
        Ok(model) => model,
        Err(services::models::ModelsError::NotFound(_)) => {
            return (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    format!("Model '{}' not found", model_name),
                    "not_found_error".to_string(),
                )),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to resolve model for embeddings");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to resolve model".to_string(),
                    "server_error".to_string(),
                )),
            )
                .into_response();
        }
    };
    let model_id = model.id;
    let organization_id = api_key.organization.id.0;

    // Extract and validate encryption headers if present
    let encryption_headers = match crate::routes::common::validate_encryption_headers(&headers) {
        Ok(headers) => headers,
        Err(err) => return err.into_response(),
    };
    let mut extra = std::collections::HashMap::new();
    insert_encryption_headers(&encryption_headers, &mut extra);
    insert_request_id_header(correlation, &mut extra);

    match app_state
        .completion_service
        .try_embeddings(organization_id, model_id, &model_name, body, extra)
        .await
    {
        Ok(response_bytes) => {
            // Minimal deserialization: extract only usage for billing
            #[derive(serde::Deserialize)]
            struct UsageExtract {
                usage: Option<UsageFields>,
            }
            #[derive(serde::Deserialize)]
            struct UsageFields {
                prompt_tokens: Option<i32>,
                total_tokens: Option<i32>,
            }

            let token_count = serde_json::from_slice::<UsageExtract>(&response_bytes)
                .ok()
                .and_then(|u| u.usage)
                .and_then(|u| u.total_tokens.or(u.prompt_tokens))
                .unwrap_or(0);

            // Token anomaly detection
            let token_count = billing::cap_reported_tokens(
                app_state.metrics_service.as_ref(),
                &model_name,
                organization_id,
                token_count,
                &billing::TokenAnomalyMessages {
                    overflow: "Provider returned unreasonable token count for embeddings - capping",
                    zero: "Provider returned zero tokens for embeddings",
                    anomaly: "Token count anomaly: Provider data quality issue detected. Recommendation: Check provider logs and configuration.",
                },
            );

            // Record usage synchronously
            let workspace_id = api_key.workspace.id.0;
            let api_key_id = match billing::billing_api_key_id(&api_key) {
                Ok(id) => id,
                Err(response) => return *response,
            };

            let inference_id = uuid::Uuid::new_v4();
            let usage_request = services::usage::RecordUsageServiceRequest {
                input_tokens: token_count,
                inference_id: Some(inference_id),
                stop_reason: Some(services::usage::StopReason::Completed),
                ..services::usage::RecordUsageServiceRequest::base(
                    organization_id,
                    workspace_id,
                    api_key_id,
                    model_id,
                    services::usage::ports::InferenceType::Embedding,
                )
            };

            if let Err(e) = app_state.usage_service.record_usage(usage_request).await {
                tracing::error!(error = %e, "Failed to record embeddings usage");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to record usage - please retry".to_string(),
                        "server_error".to_string(),
                    )),
                )
                    .into_response();
            }

            // Return raw response bytes
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(response_bytes))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(e) => {
            let (status_code, error_type, message) = match e {
                services::completions::ports::CompletionError::RateLimitExceeded(msg) => {
                    tracing::warn!("Concurrent request limit exceeded for embeddings");
                    (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", msg)
                }
                services::completions::ports::CompletionError::ProviderError {
                    status_code,
                    message,
                } => {
                    // `message` carries the provider-level detail (including the
                    // upstream URL on connection failures, where `status_code` is
                    // synthetic) — without it the log can't distinguish a backend
                    // 5xx from an unreachable SNI.
                    let classified = classify_provider_error(status_code, message.clone());
                    if classified.0.is_client_error() {
                        tracing::warn!(
                            model = %model_name,
                            upstream_status = status_code,
                            detail = %message,
                            "Embeddings rejected by upstream with client error"
                        );
                    } else {
                        tracing::error!(
                            model = %model_name,
                            upstream_status = status_code,
                            detail = %message,
                            "Embeddings provider error"
                        );
                    }
                    classified
                }
                services::completions::ports::CompletionError::InvalidModel(msg) => {
                    tracing::warn!("Embeddings model not found");
                    (StatusCode::NOT_FOUND, "not_found_error", msg)
                }
                services::completions::ports::CompletionError::ServiceOverloaded(_) => {
                    tracing::warn!("Embeddings service overloaded");
                    (
                        crate::routes::common::status_overloaded(),
                        "service_overloaded",
                        "All inference backends are overloaded. Please retry with exponential backoff.".to_string(),
                    )
                }
                _ => {
                    tracing::error!("Unexpected embeddings error");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "Embeddings request failed".to_string(),
                    )
                }
            };

            (
                status_code,
                ResponseJson(ErrorResponse::new(message, error_type.to_string())),
            )
                .into_response()
        }
    }
}
