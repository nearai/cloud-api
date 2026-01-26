use crate::{
    middleware::{auth::AuthenticatedApiKey, RequestBodyHash},
    models::*,
    routes::{api::AppState, common::map_domain_error_to_status},
};
use axum::{
    body::{Body, Bytes},
    extract::{Extension, Json, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json as ResponseJson, Response},
};
use futures::stream::StreamExt;
use services::common::encryption_headers as service_encryption_headers;
use services::completions::{
    hash_inference_id_to_uuid,
    ports::{CompletionMessage, CompletionRequest as ServiceCompletionRequest},
};
use std::convert::Infallible;
use std::sync::Arc;
use tracing::debug;
use utoipa;
use uuid::Uuid;

// Custom header for exposing the inference ID as a UUID
const HEADER_INFERENCE_ID: &str = "Inference-Id";

// Helper function to extract inference ID from first SSE chunk
fn extract_inference_id_from_sse(raw_bytes: &[u8]) -> Option<Uuid> {
    let chunk_str = match String::from_utf8(raw_bytes.to_vec()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "Invalid UTF-8 in SSE chunk, cannot extract inference ID");
            return None;
        }
    };
    let data = chunk_str.strip_prefix("data: ")?;
    let obj = serde_json::from_str::<serde_json::Value>(data.trim()).ok()?;
    let id = obj.get("id")?.as_str()?;
    Some(hash_inference_id_to_uuid(id))
}

// Helper function to extract text from MessageContent
fn extract_text_from_content(content: &Option<MessageContent>) -> String {
    match content {
        None => String::new(),
        Some(MessageContent::Text(text)) => text.clone(),
        Some(MessageContent::Parts(parts)) => {
            // Extract text from all text parts and join with newlines
            parts
                .iter()
                .filter_map(|part| match part {
                    MessageContentPart::Text { text } => Some(text.clone()),
                    _ => None, // Non-text parts should be filtered out by validation
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

// Convert HTTP ChatCompletionRequest to service CompletionRequest
fn convert_chat_request_to_service(
    request: &ChatCompletionRequest,
    user_id: Uuid,
    api_key_id: String,
    organization_id: Uuid,
    workspace_id: Uuid,
    body_hash: RequestBodyHash,
) -> ServiceCompletionRequest {
    ServiceCompletionRequest {
        model: request.model.clone(),
        messages: request
            .messages
            .iter()
            .map(|msg| CompletionMessage {
                role: msg.role.clone(),
                content: extract_text_from_content(&msg.content),
            })
            .collect(),
        max_tokens: request.max_tokens,
        temperature: request.temperature,
        top_p: request.top_p,
        stop: request.stop.clone(),
        stream: request.stream,
        user_id: user_id.into(),
        n: request.n,
        api_key_id,
        organization_id,
        workspace_id,
        metadata: None,
        body_hash: body_hash.hash.clone(),
        response_id: None, // Direct chat completions API calls don't have a response_id
        extra: request.extra.clone(),
    }
}

// Convert HTTP CompletionRequest to service CompletionRequest
fn convert_text_request_to_service(
    request: &CompletionRequest,
    user_id: Uuid,
    api_key_id: String,
    organization_id: Uuid,
    workspace_id: Uuid,
    body_hash: RequestBodyHash,
) -> ServiceCompletionRequest {
    ServiceCompletionRequest {
        model: request.model.clone(),
        messages: vec![CompletionMessage {
            role: "user".to_string(),
            content: request.prompt.clone(),
        }],
        max_tokens: request.max_tokens,
        temperature: request.temperature,
        top_p: request.top_p,
        stop: request.stop.clone(),
        stream: request.stream,
        user_id: user_id.into(),
        n: request.n,
        api_key_id,
        organization_id,
        workspace_id,
        metadata: None,
        body_hash: body_hash.hash.clone(),
        response_id: None, // Direct text completions API calls don't have a response_id
        extra: request.extra.clone(),
    }
}

/// Create chat completion
///
/// Generate AI model responses for chat conversations. Supports both streaming and non-streaming modes.
/// OpenAI-compatible endpoint.
#[utoipa::path(
    post,
    path = "/v1/chat/completions",
    tag = "Chat",
    request_body = ChatCompletionRequest,
    responses(
        (status = 200, description = "Completion generated successfully", body = ChatCompletionResponse),
        (status = 400, description = "Invalid request parameters", body = ErrorResponse),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 402, description = "Insufficient credits", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn chat_completions(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    headers: header::HeaderMap,
    Json(request): Json<ChatCompletionRequest>,
) -> axum::response::Response {
    debug!(
        "Chat completions request from api key: {:?}",
        api_key.api_key.id
    );
    debug!(
        "Request model: {}, stream: {:?}, messages: {}, org: {}, workspace: {}",
        request.model,
        request.stream,
        request.messages.len(),
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

    // Convert HTTP request to service parameters
    // Note: Names are not passed - high-cardinality data is tracked via database, not metrics
    let mut service_request = convert_chat_request_to_service(
        &request,
        api_key.api_key.created_by_user_id.0,
        api_key.api_key.id.0.clone(),
        api_key.organization.id.0,
        api_key.workspace.id.0,
        body_hash,
    );

    // Extract and validate encryption headers if present
    let encryption_headers = match crate::routes::common::validate_encryption_headers(&headers) {
        Ok(headers) => headers,
        Err(err) => return err.into_response(),
    };

    // Add validated headers to service_request.extra
    if let Some(ref signing_algo) = encryption_headers.signing_algo {
        service_request.extra.insert(
            service_encryption_headers::SIGNING_ALGO.to_string(),
            serde_json::Value::String(signing_algo.clone()),
        );
    }
    if let Some(ref client_pub_key) = encryption_headers.client_pub_key {
        service_request.extra.insert(
            service_encryption_headers::CLIENT_PUB_KEY.to_string(),
            serde_json::Value::String(client_pub_key.clone()),
        );
    }
    if let Some(ref model_pub_key) = encryption_headers.model_pub_key {
        service_request.extra.insert(
            service_encryption_headers::MODEL_PUB_KEY.to_string(),
            serde_json::Value::String(model_pub_key.clone()),
        );
    }

    // Check if streaming is requested
    if request.stream == Some(true) {
        // Call the streaming completion service
        match app_state
            .completion_service
            .create_chat_completion_stream(service_request)
            .await
        {
            Ok(stream) => {
                // Make stream peekable to extract chat_id for Inference-Id header
                let mut peekable_stream = Box::pin(stream.peekable());

                // Peek at first chunk to extract chat_id and generate Inference-Id UUID
                let inference_id = peekable_stream
                    .as_mut()
                    .peek()
                    .await
                    .and_then(|result| result.as_ref().ok())
                    .and_then(|event| extract_inference_id_from_sse(&event.raw_bytes));

                if inference_id.is_none() {
                    tracing::warn!(
                        "Could not extract inference ID from first chunk for chat completion (streaming)"
                    );
                }

                // Accumulate all SSE bytes for response hash computation
                let accumulated_bytes = Arc::new(tokio::sync::Mutex::new(Vec::new()));
                let chat_id_state = Arc::new(tokio::sync::Mutex::new(None::<String>));

                let accumulated_clone = accumulated_bytes.clone();
                let chat_id_clone = chat_id_state.clone();

                // Convert to raw bytes stream with proper SSE formatting
                let byte_stream = peekable_stream
                    .then(move |result| {
                        let accumulated_inner = accumulated_clone.clone();
                        let chat_id_inner = chat_id_clone.clone();
                        async move {
                            match result {
                                Ok(event) => {
                                    // Extract chat_id from the first chunk if available
                                    if let Ok(chunk_str) =
                                        String::from_utf8(event.raw_bytes.to_vec())
                                    {
                                        if let Some(data) = chunk_str.strip_prefix("data: ") {
                                            if let Ok(serde_json::Value::Object(obj)) =
                                                serde_json::from_str::<serde_json::Value>(
                                                    data.trim(),
                                                )
                                            {
                                                if let Some(serde_json::Value::String(id)) =
                                                    obj.get("id")
                                                {
                                                    // Capture chat_id for use in the chain combinator
                                                    // The real hash will be registered there after accumulating all bytes
                                                    let mut cid = chat_id_inner.lock().await;
                                                    if cid.is_none() {
                                                        *cid = Some(id.clone());
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // raw_bytes contains "data: {...}\n", extract just the JSON part
                                    let raw_str = String::from_utf8_lossy(&event.raw_bytes);
                                    let json_data = raw_str
                                        .trim()
                                        .strip_prefix("data: ")
                                        .unwrap_or(raw_str.trim())
                                        .to_string();
                                    tracing::debug!("Completion stream event: {}", json_data);
                                    // Format as SSE event with proper newlines
                                    let sse_bytes = Bytes::from(format!("data: {json_data}\n\n"));
                                    accumulated_inner.lock().await.extend_from_slice(&sse_bytes);
                                    Ok::<Bytes, Infallible>(sse_bytes)
                                }
                                Err(e) => {
                                    tracing::error!("Completion stream error");
                                    Ok::<Bytes, Infallible>(Bytes::from(format!(
                                        "data: error: {e}\n\n"
                                    )))
                                }
                            }
                        }
                    })
                    .chain(futures::stream::once(async move {
                        let done_bytes = Bytes::from_static(b"data: [DONE]\n\n");
                        accumulated_bytes
                            .lock()
                            .await
                            .extend_from_slice(&done_bytes);

                        Ok::<Bytes, Infallible>(done_bytes)
                    }));

                // Return raw streaming response with SSE headers
                let mut response_builder = Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header(header::CONNECTION, "keep-alive");

                // Add Inference-Id header if available
                if let Some(uuid) = inference_id {
                    response_builder = response_builder
                        .header(HEADER_INFERENCE_ID, uuid.to_string())
                        .header("Access-Control-Expose-Headers", HEADER_INFERENCE_ID);
                }

                response_builder
                    .body(Body::from_stream(byte_stream))
                    .unwrap()
            }
            Err(domain_error) => {
                let status_code = map_domain_error_to_status(&domain_error);
                (
                    status_code,
                    ResponseJson::<ErrorResponse>(domain_error.into()),
                )
                    .into_response()
            }
        }
    } else {
        // Call the non-streaming completion service
        match app_state
            .completion_service
            .create_chat_completion(service_request)
            .await
        {
            Ok(response_with_bytes) => {
                // Extract inference ID from response ID (reuse same hashing as usage tracking)
                let inference_id =
                    Some(hash_inference_id_to_uuid(&response_with_bytes.response.id));

                // Return the exact bytes from the provider for hash verification
                // This ensures clients can hash the response and compare with attestation endpoints
                let mut response_builder = Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/json");

                // Add Inference-Id header if available
                if let Some(uuid) = inference_id {
                    response_builder = response_builder
                        .header(HEADER_INFERENCE_ID, uuid.to_string())
                        .header("Access-Control-Expose-Headers", HEADER_INFERENCE_ID);
                }

                response_builder
                    .body(Body::from(response_with_bytes.raw_bytes))
                    .unwrap()
            }
            Err(domain_error) => {
                let status_code = map_domain_error_to_status(&domain_error);
                (
                    status_code,
                    ResponseJson::<ErrorResponse>(domain_error.into()),
                )
                    .into_response()
            }
        }
    }
}

/// Create text completion
///
/// Generate AI model responses for text prompts. OpenAI-compatible endpoint.
#[utoipa::path(
    post,
    path = "/v1/completions",
    tag = "Chat",
    request_body = CompletionRequest,
    responses(
        (status = 200, description = "Completion generated successfully", body = ChatCompletionResponse),
        (status = 400, description = "Invalid request parameters", body = ErrorResponse),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn completions(
    #[allow(unused)] State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    #[allow(unused)] Extension(body_hash): Extension<RequestBodyHash>,
    Json(request): Json<CompletionRequest>,
) -> axum::response::Response {
    debug!(
        "Text completions request from key: {:?}",
        api_key.api_key.id
    );
    debug!(
        "Request model: {}, stream: {:?}, org: {}, workspace: {}",
        request.model, request.stream, api_key.organization.id, api_key.workspace.id.0
    );

    return (
        StatusCode::NOT_IMPLEMENTED,
        ResponseJson(ErrorResponse::new(
            "This endpoint is not implemented".to_string(),
            "not_implemented".to_string(),
        )),
    )
        .into_response();

    #[allow(unreachable_code)]
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

    // Convert HTTP request to service parameters
    // Note: Names are not passed - high-cardinality data is tracked via database, not metrics
    let service_request = convert_text_request_to_service(
        &request,
        api_key.api_key.created_by_user_id.0,
        api_key.api_key.id.0.clone(),
        api_key.organization.id.0,
        api_key.workspace.id.0,
        body_hash,
    );

    // Call the completion service - it handles usage tracking internally
    match app_state
        .completion_service
        .create_chat_completion_stream(service_request)
        .await
    {
        Ok(_stream) => {
            unimplemented!();
        }
        Err(domain_error) => {
            let status_code = map_domain_error_to_status(&domain_error);
            (
                status_code,
                ResponseJson::<ErrorResponse>(domain_error.into()),
            )
                .into_response()
        }
    }
}

/// List available models
///
/// Returns all AI models available for completions. OpenAI-compatible endpoint.
#[utoipa::path(
    get,
    path = "/v1/models",
    tag = "Chat",
    responses(
        (status = 200, description = "List of available models", body = ModelsResponse),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn models(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
) -> Result<ResponseJson<ModelsResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("Models list request from key: {:?}", api_key.id);

    let (models, _total) = app_state
        .models_service
        .get_models_with_pricing(1000, 0)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    e.to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    let response = ModelsResponse {
        object: "list".to_string(),
        data: models
            .into_iter()
            .map(|model| {
                // Convert nano-dollars per token (scale 9) to dollars per million tokens
                // Formula: nano_dollars_per_token * 0.001 = dollars_per_million
                let pricing = ModelPricing {
                    input: (model.input_cost_per_token as f64) * 0.001,
                    output: (model.output_cost_per_token as f64) * 0.001,
                };
                ModelInfo {
                    id: model.model_name.clone(),
                    object: "model".to_string(),
                    created: 0, // No timestamp available in ModelWithPricing
                    owned_by: model.owned_by,
                    pricing: Some(pricing),
                    context_length: Some(model.context_length),
                }
            })
            .collect(),
    };
    Ok(ResponseJson(response))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_inference_id_from_sse_valid() {
        let sse_data = b"data: {\"id\":\"chatcmpl-123abc\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test-model\",\"choices\":[]}";

        let result = extract_inference_id_from_sse(sse_data);

        assert!(result.is_some());
        let uuid = result.unwrap();
        // UUID should be deterministic - same input produces same UUID
        let uuid2 = extract_inference_id_from_sse(sse_data).unwrap();
        assert_eq!(uuid, uuid2);
    }

    #[test]
    fn test_extract_inference_id_from_sse_deterministic() {
        // Test that the same chat ID always produces the same UUID
        let sse_data1 = b"data: {\"id\":\"chatcmpl-test123\",\"object\":\"chat.completion.chunk\"}";
        let sse_data2 = b"data: {\"id\":\"chatcmpl-test123\",\"object\":\"chat.completion.chunk\",\"model\":\"different\"}";

        let uuid1 = extract_inference_id_from_sse(sse_data1).unwrap();
        let uuid2 = extract_inference_id_from_sse(sse_data2).unwrap();

        // Same ID should produce same UUID even with different JSON structure
        assert_eq!(uuid1, uuid2);
    }

    #[test]
    fn test_extract_inference_id_from_sse_different_ids() {
        let sse_data1 = b"data: {\"id\":\"chatcmpl-abc123\"}";
        let sse_data2 = b"data: {\"id\":\"chatcmpl-xyz789\"}";

        let uuid1 = extract_inference_id_from_sse(sse_data1).unwrap();
        let uuid2 = extract_inference_id_from_sse(sse_data2).unwrap();

        // Different IDs should produce different UUIDs
        assert_ne!(uuid1, uuid2);
    }

    #[test]
    fn test_extract_inference_id_from_sse_missing_data_prefix() {
        let invalid_data = b"{\"id\":\"chatcmpl-123abc\"}";
        let result = extract_inference_id_from_sse(invalid_data);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_inference_id_from_sse_invalid_json() {
        let invalid_json = b"data: {invalid json}";
        let result = extract_inference_id_from_sse(invalid_json);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_inference_id_from_sse_missing_id_field() {
        let no_id = b"data: {\"object\":\"chat.completion.chunk\",\"model\":\"test\"}";
        let result = extract_inference_id_from_sse(no_id);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_inference_id_from_sse_id_not_string() {
        let id_not_string = b"data: {\"id\":12345}";
        let result = extract_inference_id_from_sse(id_not_string);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_inference_id_from_sse_invalid_utf8() {
        let invalid_utf8 = b"data: \xff\xfe{\"id\":\"test\"}";
        let result = extract_inference_id_from_sse(invalid_utf8);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_inference_id_from_sse_empty_id() {
        let empty_id = b"data: {\"id\":\"\"}";
        let result = extract_inference_id_from_sse(empty_id);
        // Empty string should still produce a valid UUID
        assert!(result.is_some());
    }
}

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
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn image_generations(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    headers: header::HeaderMap,
    Json(request): Json<crate::models::ImageGenerationRequest>,
) -> axum::response::Response {
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

    // Add validated encryption headers to extra
    if let Some(ref signing_algo) = encryption_headers.signing_algo {
        extra.insert(
            service_encryption_headers::SIGNING_ALGO.to_string(),
            serde_json::Value::String(signing_algo.clone()),
        );
    }
    if let Some(ref client_pub_key) = encryption_headers.client_pub_key {
        extra.insert(
            service_encryption_headers::CLIENT_PUB_KEY.to_string(),
            serde_json::Value::String(client_pub_key.clone()),
        );
    }
    if let Some(ref model_pub_key) = encryption_headers.model_pub_key {
        extra.insert(
            service_encryption_headers::MODEL_PUB_KEY.to_string(),
            serde_json::Value::String(model_pub_key.clone()),
        );
    }

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
        .image_generation(params, body_hash.hash.clone())
        .await
    {
        Ok(response_with_bytes) => {
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

            // Spawn async task to record usage (fire-and-forget like chat completions)
            tokio::spawn(async move {
                // Parse API key ID to UUID
                let api_key_id = match Uuid::parse_str(&api_key_id_str) {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!(error = %e, "Invalid API key ID for usage tracking");
                        return;
                    }
                };

                // Hash the provider request ID to UUID for storage
                let inference_id = Some(hash_inference_id_to_uuid(&provider_request_id));

                let usage_request = services::usage::RecordUsageServiceRequest {
                    organization_id,
                    workspace_id,
                    api_key_id,
                    model_id,
                    // Image generation doesn't have traditional token counts
                    input_tokens: 0,
                    output_tokens: 0,
                    inference_type: "image_generation".to_string(),
                    ttft_ms: None,
                    avg_itl_ms: None,
                    inference_id,
                    provider_request_id: Some(provider_request_id),
                    stop_reason: Some(services::usage::StopReason::Completed),
                    response_id: None,
                    image_count: Some(image_count),
                };

                if let Err(e) = usage_service.record_usage(usage_request).await {
                    tracing::error!(
                        error = %e,
                        %organization_id,
                        %workspace_id,
                        "Failed to record image generation usage"
                    );
                }
            });

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
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn image_edits(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
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
            Ok(Some(field)) => {
                match field.name().unwrap_or("") {
                    "image" => {
                        let bytes = match field.bytes().await {
                            Ok(b) => b.to_vec(),
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
                        };

                        // Check size limit (512 MB)
                        if bytes.len() > 512 * 1024 * 1024 {
                            return (
                                StatusCode::PAYLOAD_TOO_LARGE,
                                ResponseJson(ErrorResponse::new(
                                    "Image size exceeds maximum of 512 MB".to_string(),
                                    "invalid_request_error".to_string(),
                                )),
                            )
                                .into_response();
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
                tracing::debug!(error = %e, "Multipart parsing error");
                return (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        "Invalid multipart form data".to_string(),
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
        image: request.image,
        size: request.size,
        response_format,
    };

    // Call the inference provider pool
    match app_state
        .inference_provider_pool
        .image_edit(params, body_hash.hash.clone())
        .await
    {
        Ok(response_with_bytes) => {
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

            // Spawn async task to record usage (fire-and-forget like image generation)
            tokio::spawn(async move {
                // Parse API key ID to UUID
                let api_key_id = match Uuid::parse_str(&api_key_id_str) {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!(error = %e, "Invalid API key ID for usage tracking");
                        return;
                    }
                };

                // Hash the provider request ID to UUID for storage
                let inference_id = Some(hash_inference_id_to_uuid(&provider_request_id));

                let usage_request = services::usage::RecordUsageServiceRequest {
                    organization_id,
                    workspace_id,
                    api_key_id,
                    model_id,
                    // Image edit doesn't have traditional token counts
                    input_tokens: 0,
                    output_tokens: 0,
                    inference_type: "image_edit".to_string(),
                    ttft_ms: None,
                    avg_itl_ms: None,
                    inference_id,
                    provider_request_id: Some(provider_request_id),
                    stop_reason: Some(services::usage::StopReason::Completed),
                    response_id: None,
                    image_count: Some(image_count),
                };

                if let Err(e) = usage_service.record_usage(usage_request).await {
                    tracing::error!(
                        error = %e,
                        %organization_id,
                        %workspace_id,
                        "Failed to record image edit usage"
                    );
                }
            });

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
