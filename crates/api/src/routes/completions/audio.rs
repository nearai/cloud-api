use super::*;

/// Audio transcription endpoint
///
/// Transcribe audio files using Whisper models. Accepts audio file uploads via multipart/form-data.
/// Supports MP3, WAV, WEBM, FLAC, OGG, and M4A formats. Maximum file size: 25 MB.
///
/// **Request Body (multipart/form-data):**
/// All fields should be provided as text values or files as indicated in the schema.
#[utoipa::path(
    post,
    path = "/v1/audio/transcriptions",
    tag = "Audio",
    request_body(content = AudioTranscriptionRequestSchema, content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "Successful transcription", content(
            (AudioTranscriptionResponse = "application/json"),
            (String = "text/plain")
        )),
        (status = 400, description = "Invalid request (empty file, unsupported format, file too large)", body = ErrorResponse),
        (status = 401, description = "Unauthorized (missing or invalid API key)", body = ErrorResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse),
        (status = 429, description = "Rate limited or overloaded — retry with backoff. Check error.type: rate_limit_exceeded or service_overloaded.", body = ErrorResponse),
    ),
    security(("ApiKeyAuth" = []))
)]
pub async fn audio_transcriptions(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    Extension(correlation): Extension<RequestCorrelation>,
    headers: header::HeaderMap,
    requested_model: RequestedModel,
    mut multipart: Multipart,
) -> axum::response::Response {
    debug!(
        "Audio transcription request from api key: {:?}",
        api_key.api_key.id
    );

    // Parse multipart form fields
    let mut model: Option<String> = None;
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut filename: Option<String> = None;
    let mut language: Option<String> = None;
    let mut response_format: Option<String> = None;
    let mut temperature: Option<f32> = None;
    let mut timestamp_granularities: Option<Vec<String>> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        let field_name = match field.name() {
            Some(name) => name.to_string(),
            None => {
                tracing::warn!("Multipart field name is missing");
                return (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        "Missing field name in multipart request".to_string(),
                        "invalid_request_error".to_string(),
                    )),
                )
                    .into_response();
            }
        };

        match field_name.as_str() {
            "file" => {
                filename = field.file_name().map(|s| s.to_string());
                match field.bytes().await {
                    Ok(bytes) => file_bytes = Some(bytes.to_vec()),
                    Err(_) => {
                        // Don't log error details - may contain customer data
                        tracing::error!("Failed to read file field");
                        return (
                            StatusCode::BAD_REQUEST,
                            ResponseJson(ErrorResponse::new(
                                "Failed to read audio file".to_string(),
                                "invalid_request_error".to_string(),
                            )),
                        )
                            .into_response();
                    }
                }
            }
            "model" => {
                if let Ok(value) = field.text().await {
                    model = Some(value);
                }
            }
            "language" => {
                if let Ok(value) = field.text().await {
                    language = Some(value);
                }
            }
            "response_format" => {
                if let Ok(value) = field.text().await {
                    response_format = Some(value);
                }
            }
            "temperature" => {
                if let Ok(value) = field.text().await {
                    if let Ok(temp) = value.parse::<f32>() {
                        temperature = Some(temp);
                    }
                }
            }
            "timestamp_granularities[]" | "timestamp_granularities" => {
                if let Ok(value) = field.text().await {
                    timestamp_granularities
                        .get_or_insert_with(Vec::new)
                        .extend(value.split(',').map(|s| s.trim().to_string()));
                }
            }
            _ => {
                tracing::debug!("Skipping unknown field: {}", field_name);
            }
        }
    }

    // Construct request and validate
    let request = crate::models::AudioTranscriptionRequest {
        model: model.unwrap_or_default(),
        file_bytes: file_bytes.unwrap_or_default(),
        filename: filename.unwrap_or_else(|| "audio.mp3".to_string()),
        language,
        response_format,
        temperature,
        timestamp_granularities,
    };
    requested_model.set(&request.model);

    debug!(
        "Audio transcription: model={}, filename={}, file_size_kb={}, org={}, workspace={}",
        request.model,
        request.filename,
        request.file_bytes.len() / 1024,
        api_key.organization.id,
        api_key.workspace.id.0
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

    // Resolve model to get UUID for usage tracking (handles aliases like chat_completions)
    let model = match app_state
        .models_service
        .resolve_and_get_model(&request.model)
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
            tracing::error!(error = %e, "Failed to resolve model for audio transcription");
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
    let model_name = request.model.clone();
    let organization_id = api_key.organization.id.0;

    // Extract and validate encryption headers if present
    let encryption_headers = match crate::routes::common::validate_encryption_headers(&headers) {
        Ok(headers) => headers,
        Err(err) => return err.into_response(),
    };

    // Convert API request to provider params
    let mut extra = std::collections::HashMap::new();
    insert_encryption_headers(&encryption_headers, &mut extra);
    insert_request_id_header(correlation, &mut extra);

    let requested_response_format = request.response_format.clone();
    let provider_response_format = match requested_response_format.as_deref() {
        // Keep provider parsing stable and preserve duration-based billing by
        // requesting verbose JSON, then returning plain text at the API boundary
        // after usage is recorded.
        Some("text") => Some("verbose_json".to_string()),
        _ => request.response_format.clone(),
    };

    let params = inference_providers::AudioTranscriptionParams {
        model: model_name.clone(),
        file_bytes: request.file_bytes,
        filename: request.filename,
        language: request
            .language
            .map(|language| crate::models::normalize_audio_transcription_language(&language)),
        response_format: provider_response_format,
        temperature: request.temperature,
        timestamp_granularities: request.timestamp_granularities,
        extra,
    };

    // Call completion service which handles concurrent request limiting
    match app_state
        .completion_service
        .audio_transcription(
            organization_id,
            model_id,
            &model_name,
            params,
            body_hash.hash.clone(),
        )
        .await
    {
        Ok(response) => {
            // Record usage for audio transcription SYNCHRONOUSLY before returning response
            // Critical for financial accuracy: if usage recording fails, client gets 500 error
            // rather than 200 success. This prevents lost revenue and maintains audit trail.
            // Bill by audio duration in seconds (use input_tokens field)
            let workspace_id = api_key.workspace.id.0;
            let api_key_id_str = api_key.api_key.id.0.clone();

            // Clamp duration to valid range [0, i32::MAX] to prevent overflow and negative values
            let duration_seconds = response
                .duration
                .unwrap_or(0.0)
                .max(0.0)
                .min(i32::MAX as f64)
                .ceil() as i32;

            // Parse API key ID to UUID
            let api_key_id = match uuid::Uuid::parse_str(&api_key_id_str) {
                Ok(id) => id,
                Err(_) => {
                    tracing::error!("Invalid API key ID for usage tracking");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ResponseJson(ErrorResponse::new(
                            "Failed to record usage - invalid API key format".to_string(),
                            "server_error".to_string(),
                        )),
                    )
                        .into_response();
                }
            };

            let inference_id = uuid::Uuid::new_v4();
            let usage_request = services::usage::RecordUsageServiceRequest {
                input_tokens: duration_seconds, // Bill by duration in seconds
                inference_id: Some(inference_id),
                stop_reason: Some(services::usage::StopReason::Completed),
                ..services::usage::RecordUsageServiceRequest::base(
                    services::usage::BillingIdentity {
                        organization_id,
                        workspace_id,
                        api_key_id,
                        model_id,
                    },
                    services::usage::ports::InferenceType::AudioTranscription,
                )
            };

            // Record usage synchronously - fail the request if usage recording fails
            if let Err(e) = app_state.usage_service.record_usage(usage_request).await {
                tracing::error!(
                    error = %e,
                    %organization_id,
                    %workspace_id,
                    "Failed to record audio transcription usage"
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to record usage - please retry".to_string(),
                        "server_error".to_string(),
                    )),
                )
                    .into_response();
            }

            tracing::info!(
                %organization_id,
                %workspace_id,
                duration_seconds,
                "Audio transcription completed and usage recorded successfully"
            );

            if requested_response_format.as_deref() == Some("text") {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    response.text,
                )
                    .into_response()
            } else {
                let response_body = crate::models::AudioTranscriptionResponse::from(response);
                (StatusCode::OK, ResponseJson(response_body)).into_response()
            }
        }
        Err(e) => {
            let (status_code, error_type, message) = match e {
                services::completions::ports::CompletionError::RateLimitExceeded(msg) => {
                    tracing::warn!("Concurrent request limit exceeded for audio transcription");
                    (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", msg)
                }
                services::completions::ports::CompletionError::ProviderError {
                    status_code,
                    message,
                } => {
                    let http_status = StatusCode::from_u16(status_code)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    if inference_providers::is_client_audio_input_status(status_code) {
                        // A genuine client-input 4xx (400/413/415/422 — malformed,
                        // unsupported, empty, or oversized audio). Not a service
                        // fault: log at warn so a burst of bad uploads does not
                        // page on-call, and return an actionable
                        // invalid_request_error instead of a misleading
                        // "server_error / try again later" (which also encourages
                        // doomed retries). Mirrors the chat path in
                        // completion_stream_error_openai_type.
                        //
                        // The pool classifies provider-infra failures (401/403
                        // creds, 404 missing route, 408 timeout, 429, 5xx) into
                        // server errors / RateLimitExceeded before they reach
                        // here, so they take the `else` branch (or the arm above)
                        // and still page. This predicate is the shared source of
                        // truth for the client-input vs infra split.
                        tracing::warn!(
                            status_code,
                            %organization_id,
                            workspace_id = %api_key.workspace.id.0,
                            detail = %message,
                            "Audio transcription rejected by provider (client input)"
                        );
                        (
                            http_status,
                            "invalid_request_error",
                            "Audio could not be processed. Ensure the file is valid, \
                             non-empty audio in a supported format and within size and \
                             duration limits."
                                .to_string(),
                        )
                    } else {
                        tracing::error!(
                            status_code,
                            %organization_id,
                            workspace_id = %api_key.workspace.id.0,
                            detail = %message,
                            "Audio transcription provider error"
                        );
                        (
                            http_status,
                            "server_error",
                            "Audio transcription failed. Please try again later.".to_string(),
                        )
                    }
                }
                services::completions::ports::CompletionError::ServiceOverloaded(_) => {
                    tracing::warn!("Audio transcription service overloaded");
                    (
                        crate::routes::common::status_overloaded(),
                        "service_overloaded",
                        "All inference backends are overloaded. Please retry with exponential backoff.".to_string(),
                    )
                }
                _ => {
                    tracing::error!("Unexpected audio transcription error");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "Audio transcription failed".to_string(),
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
