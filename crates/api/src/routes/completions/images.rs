use super::*;

/// Generate images from text prompt
///
/// Generate images using an AI model from a text description. OpenAI-compatible endpoint.
#[utoipa::path(
    post,
    path = "/v1/images/generations",
    tag = "Images",
    request_body = ImageGenerationRequest,
    responses(
        (status = 200, description = "Image generated successfully", body = ImageGenerationResponse),
        (status = 400, description = "Invalid request parameters", body = ErrorResponse),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse),
        (status = 429, description = "Rate limited or overloaded — retry with backoff. Check error.type: rate_limit_exceeded or service_overloaded.", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn image_generations(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    Extension(correlation): Extension<RequestCorrelation>,
    headers: header::HeaderMap,
    requested_model: RequestedModel,
    OpenAiJson(request): OpenAiJson<crate::models::ImageGenerationRequest>,
) -> axum::response::Response {
    requested_model.set(&request.model);
    debug!(
        "Image generation request from api key: {:?}",
        api_key.api_key.id
    );
    debug!(
        "Image generation request: model={}, org={}, workspace={}",
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

    // Extract and validate encryption headers if present
    let encryption_headers = match crate::routes::common::validate_encryption_headers(&headers) {
        Ok(headers) => headers,
        Err(err) => return err.into_response(),
    };

    // Resolve model to get UUID for usage tracking
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
                    "invalid_request_error".to_string(),
                )),
            )
                .into_response();
        }
        Err(e) => {
            tracing::debug!(error = %e, "Failed to resolve model for image generation");
            tracing::warn!("Image generation: model resolution failed");
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

    // Validate and enforce response_format for verifiable models
    // Verifiable models (attestation_supported = true) only support "b64_json" format
    // Default to "b64_json" if not specified to prevent downstream server from applying "url" default
    let response_format = if model.attestation_supported {
        match &request.response_format {
            Some(format) if format == "b64_json" => Some(format.clone()),
            Some(format) => {
                return (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        format!(
                            "response_format '{}' is not supported for verifiable models. Only 'b64_json' is supported.",
                            format
                        ),
                        "invalid_request_error".to_string(),
                    )),
                )
                    .into_response();
            }
            None => Some("b64_json".to_string()), // Default to b64_json for verifiable models
        }
    } else {
        request.response_format.clone()
    };

    // Convert API request to provider params
    let mut extra = std::collections::HashMap::new();
    insert_encryption_headers(&encryption_headers, &mut extra);
    insert_request_id_header(correlation, &mut extra);

    let params = inference_providers::ImageGenerationParams {
        model: request.model.clone(),
        prompt: request.prompt.clone(),
        n: request.n,
        size: request.size.clone(),
        response_format,
        quality: request.quality.clone(),
        style: request.style.clone(),
        extra,
    };

    // Call the inference provider pool
    match app_state
        .inference_provider_pool
        .image_generation_with_attribution(params, body_hash.hash.clone())
        .await
    {
        Ok(attributed_response) => {
            let response_with_bytes = attributed_response.response;
            let provider_attribution = attributed_response.provider_attribution;
            // Store attestation signature for image generation (same pattern as chat completions)
            let attestation_service = app_state.attestation_service.clone();
            let image_id_for_sig = response_with_bytes.response.id.clone();
            tokio::spawn(async move {
                if let Err(e) = attestation_service
                    .store_chat_signature_from_provider(&image_id_for_sig)
                    .await
                {
                    tracing::debug!(error = %e, "Failed to store image generation signature");
                    tracing::warn!("Image generation: signature storage failed");
                } else {
                    tracing::debug!(image_id = %image_id_for_sig, "Stored signature for image generation");
                }
            });

            // Record usage for image generation
            let organization_id = api_key.organization.id.0;
            let workspace_id = api_key.workspace.id.0;
            let api_key_id_str = api_key.api_key.id.0.clone();
            let model_id = model.id;
            let image_count = match i32::try_from(response_with_bytes.response.data.len()) {
                Ok(count) => count,
                Err(_) => {
                    tracing::error!("Too many images in provider response, cannot fit in i32 for usage tracking");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ResponseJson(ErrorResponse::new(
                            "Internal error: too many images in provider response".to_string(),
                            "server_error".to_string(),
                        )),
                    )
                        .into_response();
                }
            };
            let provider_request_id = response_with_bytes.response.id.clone();
            let usage_service = app_state.usage_service.clone();

            // Parse API key ID early for usage recording
            let api_key_id = match Uuid::parse_str(&api_key_id_str) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, "Invalid API key ID for usage tracking");
                    // Still return response but log error
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(response_with_bytes.raw_bytes))
                        .unwrap();
                }
            };

            let usage_request = build_image_usage_request(ImageUsageRecord {
                organization_id,
                workspace_id,
                api_key_id,
                model_id,
                provider_request_id: &provider_request_id,
                image_count,
                provider_attribution,
                inference_type: services::usage::InferenceType::ImageGeneration,
            });
            record_usage_with_sync_fallback(usage_service, usage_request, "Image generation").await;

            // Return the exact bytes from the provider for hash verification
            // This ensures clients can hash the response and compare with attestation endpoints
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(response_with_bytes.raw_bytes))
                .unwrap()
        }
        Err(e) => {
            // Log provider errors at debug level to avoid exposing infrastructure details in production
            tracing::debug!(error = %e, "Image generation failed");

            // Map error to appropriate status code and sanitized message
            let (status_code, message) = match &e {
                inference_providers::ImageGenerationError::GenerationError(msg) => {
                    // Check if it's a model not found error
                    if msg.contains("not found") || msg.contains("does not exist") {
                        (StatusCode::NOT_FOUND, "Model not found".to_string())
                    } else {
                        tracing::warn!("Image generation error: service unavailable");
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "Image generation failed".to_string(),
                        )
                    }
                }
                inference_providers::ImageGenerationError::HttpError { status_code, .. } => {
                    // Map HTTP status codes appropriately
                    let code = StatusCode::from_u16(*status_code)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    let msg = match code {
                        StatusCode::NOT_FOUND => "Model not found".to_string(),
                        StatusCode::BAD_REQUEST => "Invalid request".to_string(),
                        StatusCode::TOO_MANY_REQUESTS => "Rate limit exceeded".to_string(),
                        _ => {
                            tracing::warn!(
                                http_status = *status_code,
                                "Image generation HTTP error"
                            );
                            "Image generation failed".to_string()
                        }
                    };
                    (code, msg)
                }
            };

            (
                status_code,
                ResponseJson(ErrorResponse::new(message, "server_error".to_string())),
            )
                .into_response()
        }
    }
}

/// Edit images from a text prompt and image
///
/// Edit images using an AI model from an image and text description. OpenAI-compatible endpoint.
///
/// **Request Body (multipart/form-data):**
/// All fields should be provided as text values or files as indicated in the schema.
#[utoipa::path(
    post,
    path = "/v1/images/edits",
    tag = "Images",
    request_body(content = ImageEditRequestSchema, content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "Image edited successfully", body = ImageGenerationResponse),
        (status = 400, description = "Invalid request parameters", body = ErrorResponse),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 413, description = "Payload too large", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse),
        (status = 429, description = "Rate limited or overloaded — retry with backoff. Check error.type: rate_limit_exceeded or service_overloaded.", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn image_edits(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    requested_model: RequestedModel,
    mut multipart: axum::extract::Multipart,
) -> axum::response::Response {
    debug!("Image edit request from api key: {:?}", api_key.api_key.id);

    let mut model = String::new();
    let mut prompt = String::new();
    let mut image: Option<Vec<u8>> = None;
    let mut size: Option<String> = None;
    let mut response_format: Option<String> = None;

    // Parse multipart form data
    loop {
        match multipart.next_field().await {
            Ok(Some(mut field)) => {
                match field.name().unwrap_or("") {
                    "image" => {
                        // Use streaming validation to detect oversized payloads before buffering entirely in memory.
                        // This prevents memory exhaustion DoS attacks where attackers send payloads larger than MAX_FILE_SIZE.
                        let mut bytes = Vec::new();
                        let mut size = 0usize;

                        loop {
                            match field.chunk().await {
                                Ok(Some(chunk)) => {
                                    size += chunk.len();
                                    // Reject early if size exceeds limit, before allocating memory
                                    if size > MAX_FILE_SIZE {
                                        return (
                                            StatusCode::PAYLOAD_TOO_LARGE,
                                            ResponseJson(ErrorResponse::new(
                                                "Image size exceeds maximum of 512 MB".to_string(),
                                                "invalid_request_error".to_string(),
                                            )),
                                        )
                                            .into_response();
                                    }
                                    bytes.extend_from_slice(&chunk);
                                }
                                Ok(None) => break,
                                Err(e) => {
                                    return (
                                        StatusCode::BAD_REQUEST,
                                        ResponseJson(ErrorResponse::new(
                                            format!("Failed to read image: {}", e),
                                            "invalid_request_error".to_string(),
                                        )),
                                    )
                                        .into_response();
                                }
                            }
                        }

                        image = Some(bytes);
                    }
                    "model" => match field.text().await {
                        Ok(text) => model = text,
                        Err(e) => {
                            return (
                                StatusCode::BAD_REQUEST,
                                ResponseJson(ErrorResponse::new(
                                    format!("Failed to read model: {}", e),
                                    "invalid_request_error".to_string(),
                                )),
                            )
                                .into_response();
                        }
                    },
                    "prompt" => match field.text().await {
                        Ok(text) => prompt = text,
                        Err(e) => {
                            return (
                                StatusCode::BAD_REQUEST,
                                ResponseJson(ErrorResponse::new(
                                    format!("Failed to read prompt: {}", e),
                                    "invalid_request_error".to_string(),
                                )),
                            )
                                .into_response();
                        }
                    },
                    "size" => match field.text().await {
                        Ok(text) => size = Some(text),
                        Err(e) => {
                            return (
                                StatusCode::BAD_REQUEST,
                                ResponseJson(ErrorResponse::new(
                                    format!("Failed to read size: {}", e),
                                    "invalid_request_error".to_string(),
                                )),
                            )
                                .into_response();
                        }
                    },
                    "response_format" => match field.text().await {
                        Ok(text) => response_format = Some(text),
                        Err(e) => {
                            return (
                                StatusCode::BAD_REQUEST,
                                ResponseJson(ErrorResponse::new(
                                    format!("Failed to read response_format: {}", e),
                                    "invalid_request_error".to_string(),
                                )),
                            )
                                .into_response();
                        }
                    },
                    _ => {
                        // Ignore unknown fields
                    }
                }
            }
            Ok(None) => break, // All fields read successfully
            Err(e) => {
                // Multipart parsing error (malformed boundary, invalid encoding, etc.)
                let (status, error_message) = analyze_multipart_error(&e);
                tracing::debug!(error = %e, message = %error_message, "Multipart parsing error");
                return (
                    status,
                    ResponseJson(ErrorResponse::new(
                        error_message,
                        "invalid_request_error".to_string(),
                    )),
                )
                    .into_response();
            }
        }
    }

    // Fail fast if image is missing
    let image = match image {
        Some(img) => img,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    "Missing required field: image".to_string(),
                    "invalid_request_error".to_string(),
                )),
            )
                .into_response();
        }
    };

    // Build the request
    let request = crate::models::ImageEditRequest {
        model,
        prompt,
        image,
        size,
        response_format,
    };
    requested_model.set(&request.model);

    debug!(
        "Image edit request: model={}, org={}, workspace={}",
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

    // Resolve model to get UUID for usage tracking
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
                    "invalid_request_error".to_string(),
                )),
            )
                .into_response();
        }
        Err(e) => {
            tracing::debug!(error = %e, "Failed to resolve model for image edit");
            tracing::warn!("Image edit: model resolution failed");
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

    // Validate and enforce response_format for verifiable models
    // Verifiable models (attestation_supported = true) only support "b64_json" format
    // Default to "b64_json" if not specified to prevent downstream server from applying "url" default
    let response_format = if model.attestation_supported {
        match &request.response_format {
            Some(format) if format == "b64_json" => Some(format.clone()),
            Some(format) => {
                return (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        format!(
                            "response_format '{}' is not supported for verifiable models. Only 'b64_json' is supported.",
                            format
                        ),
                        "invalid_request_error".to_string(),
                    )),
                )
                    .into_response();
            }
            None => Some("b64_json".to_string()), // Default to b64_json for verifiable models
        }
    } else {
        request.response_format.clone()
    };

    // Convert API request to provider params
    let params = inference_providers::ImageEditParams {
        model: request.model.clone(),
        prompt: request.prompt.clone(),
        image: Arc::new(request.image),
        size: request.size,
        response_format,
    };

    // Call the inference provider pool
    match app_state
        .inference_provider_pool
        .image_edit_with_attribution(params, body_hash.hash.clone())
        .await
    {
        Ok(attributed_response) => {
            let response_with_bytes = attributed_response.response;
            let provider_attribution = attributed_response.provider_attribution;
            // Store attestation signature for image edit (same pattern as image generation)
            let attestation_service = app_state.attestation_service.clone();
            let image_id_for_sig = response_with_bytes.response.id.clone();
            tokio::spawn(async move {
                if let Err(e) = attestation_service
                    .store_chat_signature_from_provider(&image_id_for_sig)
                    .await
                {
                    tracing::debug!(error = %e, "Failed to store image edit signature");
                    tracing::warn!("Image edit: signature storage failed");
                } else {
                    tracing::debug!(image_id = %image_id_for_sig, "Stored signature for image edit");
                }
            });

            // Record usage for image edit
            let organization_id = api_key.organization.id.0;
            let workspace_id = api_key.workspace.id.0;
            let api_key_id_str = api_key.api_key.id.0.clone();
            let model_id = model.id;
            let image_count = match i32::try_from(response_with_bytes.response.data.len()) {
                Ok(count) => count,
                Err(_) => {
                    tracing::error!("Too many images in provider response, cannot fit in i32 for usage tracking");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ResponseJson(ErrorResponse::new(
                            "Internal error: too many images in provider response".to_string(),
                            "server_error".to_string(),
                        )),
                    )
                        .into_response();
                }
            };
            let provider_request_id = response_with_bytes.response.id.clone();
            let usage_service = app_state.usage_service.clone();

            // Parse API key ID early for usage recording
            let api_key_id = match Uuid::parse_str(&api_key_id_str) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, "Invalid API key ID for usage tracking");
                    // Still return response but log error
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(response_with_bytes.raw_bytes))
                        .unwrap();
                }
            };

            let usage_request = build_image_usage_request(ImageUsageRecord {
                organization_id,
                workspace_id,
                api_key_id,
                model_id,
                provider_request_id: &provider_request_id,
                image_count,
                provider_attribution,
                inference_type: services::usage::InferenceType::ImageEdit,
            });
            record_usage_with_sync_fallback(usage_service, usage_request, "Image edit").await;

            // Return the exact bytes from the provider for hash verification
            // This ensures clients can hash the response and compare with attestation endpoints
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(response_with_bytes.raw_bytes))
                .unwrap()
        }
        Err(e) => {
            // Log provider errors at debug level to avoid exposing infrastructure details in production
            tracing::debug!(error = %e, "Image edit failed");

            // Map error to appropriate status code and sanitized message
            let (status_code, message) = match &e {
                inference_providers::ImageEditError::EditError(msg) => {
                    // Check if it's a model not found error
                    if msg.contains("not found") || msg.contains("does not exist") {
                        (StatusCode::NOT_FOUND, "Model not found".to_string())
                    } else {
                        tracing::warn!("Image edit error: service unavailable");
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "Image edit failed".to_string(),
                        )
                    }
                }
                inference_providers::ImageEditError::HttpError { status_code, .. } => {
                    // Map HTTP status codes appropriately
                    let code = StatusCode::from_u16(*status_code)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    let msg = match code {
                        StatusCode::NOT_FOUND => "Model not found".to_string(),
                        StatusCode::BAD_REQUEST => "Invalid request".to_string(),
                        StatusCode::TOO_MANY_REQUESTS => "Rate limit exceeded".to_string(),
                        _ => {
                            tracing::warn!(http_status = *status_code, "Image edit HTTP error");
                            "Image edit failed".to_string()
                        }
                    };
                    (code, msg)
                }
            };

            (
                status_code,
                ResponseJson(ErrorResponse::new(message, "server_error".to_string())),
            )
                .into_response()
        }
    }
}
