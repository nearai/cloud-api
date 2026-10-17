use super::*;

/// Text similarity scoring endpoint
///
/// Scores the similarity between two texts using a scoring/ranking model.
///
/// **Concurrent Request Limits:** Each organization has a per-model concurrent request limit (default: 64).
/// When the limit is reached, new requests will fail with 429 status code. Wait for in-flight requests to complete before retrying.
#[utoipa::path(
    post,
    path = "/v1/score",
    tag = "Score",
    request_body = crate::models::ScoreRequest,
    responses(
        (status = 200, description = "Successful score", body = crate::models::ScoreResponse),
        (status = 400, description = "Invalid request (empty texts, invalid model, etc.)", body = ErrorResponse),
        (status = 401, description = "Unauthorized (missing or invalid API key)", body = ErrorResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 429, description = "Rate limited or overloaded — retry with backoff. Check error.type: rate_limit_exceeded or service_overloaded.", body = ErrorResponse),
        (status = 500, description = "Server error (billing failure, provider error)", body = ErrorResponse),
    ),
    security(("ApiKeyAuth" = []))
)]
pub async fn score(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    Extension(correlation): Extension<RequestCorrelation>,
    headers: header::HeaderMap,
    requested_model: RequestedModel,
    OpenAiJson(request): OpenAiJson<crate::models::ScoreRequest>,
) -> axum::response::Response {
    requested_model.set(&request.model);
    debug!(
        "Score request: model={}, org={}, workspace={}",
        request.model, api_key.organization.id, api_key.workspace.id.0
    );

    // Validate the request
    if let Err(error) = request.validate() {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                error,
                "invalid_request_error".to_string(),
            )),
        )
            .into_response();
    }

    // Resolve model to get UUID for usage tracking - fail fast if not found
    // Models must be registered in the database to be available for use
    let model = match app_state
        .models_service
        .get_model_by_name(&request.model)
        .await
    {
        Ok(model) => model,
        Err(services::models::ModelsError::NotFound(_)) => {
            return (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    format!("Model '{}' not found", request.model),
                    "not_found_error".to_string(),
                )),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to resolve model for score");
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

    // Call completion service which handles concurrent request limiting
    // Each organization has a per-model concurrent request limit (default: 64 concurrent requests).
    // This prevents resource exhaustion and ensures fair usage. Returns 429 if limit exceeded.
    match app_state
        .completion_service
        .try_score(
            organization_id,
            model_id,
            &request.model,
            body_hash.hash.clone(),
            {
                // Extract and validate encryption headers if present
                let encryption_headers =
                    match crate::routes::common::validate_encryption_headers(&headers) {
                        Ok(headers) => headers,
                        Err(err) => return err.into_response(),
                    };
                let mut extra = std::collections::HashMap::new();
                insert_encryption_headers(&encryption_headers, &mut extra);
                insert_request_id_header(correlation, &mut extra);

                inference_providers::ScoreParams {
                    model: request.model.clone(),
                    text_1: request.text_1.clone(),
                    text_2: request.text_2.clone(),
                    extra,
                }
            },
        )
        .await
    {
        Ok(response) => {
            // Record usage for score SYNCHRONOUSLY to ensure billing accuracy
            // This is critical for revenue tracking and must complete before returning response
            let organization_id = api_key.organization.id.0;
            let workspace_id = api_key.workspace.id.0;

            // Parse API key ID to UUID
            let api_key_id = match billing::billing_api_key_id(&api_key) {
                Ok(id) => id,
                Err(response) => return *response,
            };

            let inference_id = hash_inference_id_to_uuid(&response.id);

            // Score requests don't have traditional token counts - use input token count as 1
            let usage_request = services::usage::ports::RecordUsageServiceRequest {
                input_tokens: 1,
                inference_id: Some(inference_id),
                stop_reason: Some(services::usage::StopReason::Completed),
                ..services::usage::ports::RecordUsageServiceRequest::base(
                    organization_id,
                    workspace_id,
                    api_key_id,
                    model_id,
                    services::usage::ports::InferenceType::Score,
                )
            };

            // Record usage with timeout to prevent blocking responses
            match tokio::time::timeout(
                Duration::from_secs(USAGE_RECORDING_TIMEOUT_SECS),
                app_state.usage_service.record_usage(usage_request.clone()),
            )
            .await
            {
                Ok(Ok(_)) => ResponseJson(response).into_response(),
                Ok(Err(e)) => {
                    tracing::warn!(
                        error = %e,
                        model_id = %model_id,
                        "Failed to record usage synchronously, retrying async"
                    );
                    let usage_service_clone = app_state.usage_service.clone();
                    tokio::spawn(async move {
                        if let Err(e) = usage_service_clone.record_usage(usage_request).await {
                            tracing::error!(
                                error = %e,
                                model_id = %model_id,
                                "Failed to record score usage in async retry"
                            );
                        }
                    });
                    ResponseJson(response).into_response()
                }
                Err(_timeout) => {
                    // Recording timed out, fall back to async retry to avoid blocking client
                    tracing::warn!(
                        model_id = %model_id,
                        "Score usage recording timed out, retrying async"
                    );
                    let usage_service_clone = app_state.usage_service.clone();
                    tokio::spawn(async move {
                        if let Err(e) = usage_service_clone.record_usage(usage_request).await {
                            tracing::error!(
                                error = %e,
                                model_id = %model_id,
                                "Failed to record score usage in async retry"
                            );
                        }
                    });
                    ResponseJson(response).into_response()
                }
            }
        }
        Err(e) => {
            let (status_code, error_type, message) = match e {
                services::completions::ports::CompletionError::RateLimitExceeded(msg) => {
                    tracing::warn!("Concurrent request limit exceeded for score");
                    (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", msg)
                }
                services::completions::ports::CompletionError::ProviderError {
                    status_code,
                    message,
                } => {
                    tracing::error!(
                        model = %request.model,
                        upstream_status = status_code,
                        detail = %message,
                        "Score provider error"
                    );
                    let http_status = StatusCode::from_u16(status_code)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    (
                        http_status,
                        "server_error",
                        "Scoring failed. Please try again later.".to_string(),
                    )
                }
                services::completions::ports::CompletionError::InvalidModel(msg) => {
                    tracing::warn!("Score model not found");
                    (StatusCode::NOT_FOUND, "not_found_error", msg)
                }
                services::completions::ports::CompletionError::ServiceOverloaded(_) => {
                    tracing::warn!("Score service overloaded");
                    (
                        crate::routes::common::status_overloaded(),
                        "service_overloaded",
                        "All inference backends are overloaded. Please retry with exponential backoff.".to_string(),
                    )
                }
                _ => {
                    tracing::error!("Unexpected score error");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "Scoring failed".to_string(),
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
