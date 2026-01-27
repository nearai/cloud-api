use crate::{
    middleware::{auth::AuthenticatedApiKey, RequestBodyHash},
    models::*,
    routes::{api::AppState, common::map_domain_error_to_status},
};
use axum::{
    body::{Body, Bytes},
    extract::{Extension, Json, Multipart, State},
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
                    architecture: ModelArchitecture::from_options(
                        model.input_modalities,
                        model.output_modalities,
                    ),
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
            tracing::error!(error = %e, "Failed to resolve model for image generation");
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
                    tracing::error!(error = %e, "Failed to store image generation signature");
                } else {
                    tracing::debug!(image_id = %image_id_for_sig, "Stored signature for image generation");
                }
            });

            // Record usage for image generation
            let organization_id = api_key.organization.id.0;
            let workspace_id = api_key.workspace.id.0;
            let api_key_id_str = api_key.api_key.id.0.clone();
            let model_id = model.id;
            let image_count = response_with_bytes.response.data.len() as i32;
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
            // Log the full error internally but return a sanitized message to the client
            tracing::error!(error = %e, "Image generation failed");

            // Map error to appropriate status code and sanitized message
            let (status_code, message) = match &e {
                inference_providers::ImageGenerationError::GenerationError(msg) => {
                    // Check if it's a model not found error
                    if msg.contains("not found") || msg.contains("does not exist") {
                        (StatusCode::NOT_FOUND, "Model not found".to_string())
                    } else {
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
                        _ => "Image generation failed".to_string(),
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

/// Audio transcription endpoint
///
/// Transcribe audio files using Whisper models. Accepts audio file uploads via multipart/form-data.
/// Supports MP3, WAV, WEBM, FLAC, OGG, and M4A formats. Maximum file size: 25 MB.
#[utoipa::path(
    post,
    path = "/v1/audio/transcriptions",
    tag = "Audio",
    request_body(content = AudioTranscriptionRequestSchema, content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "Successful transcription", body = AudioTranscriptionResponse),
        (status = 400, description = "Invalid request (empty file, unsupported format, file too large)", body = ErrorResponse),
        (status = 401, description = "Unauthorized (missing or invalid API key)", body = ErrorResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse),
    ),
    security(("ApiKeyAuth" = []))
)]
pub async fn audio_transcriptions(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
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
        let field_name = field.name().unwrap_or("").to_string();

        match field_name.as_str() {
            "file" => {
                filename = field.file_name().map(|s| s.to_string());
                match field.bytes().await {
                    Ok(bytes) => file_bytes = Some(bytes.to_vec()),
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to read file field");
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
                    timestamp_granularities =
                        Some(value.split(',').map(|s| s.trim().to_string()).collect());
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

    // Convert API request to provider params
    let params = inference_providers::AudioTranscriptionParams {
        model: model_name.clone(),
        file_bytes: request.file_bytes,
        filename: request.filename,
        language: request.language,
        response_format: request.response_format,
        temperature: request.temperature,
        timestamp_granularities: request.timestamp_granularities,
        extra: std::collections::HashMap::new(),
    };

    // Call inference provider pool directly (concurrent limiting is handled by the pool)
    match app_state
        .inference_provider_pool
        .audio_transcription(params, body_hash.hash.clone())
        .await
    {
        Ok(response) => {
            // Record usage for audio transcription SYNCHRONOUSLY
            // Bill by audio duration in seconds (use input_tokens field)
            let duration_seconds = response.duration.unwrap_or(0.0).ceil() as i32;

            let workspace_id = api_key.workspace.id.0;
            let api_key_id_str = api_key.api_key.id.0.clone();
            let api_key_id = match uuid::Uuid::parse_str(&api_key_id_str) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, "Invalid API key ID for usage tracking");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ResponseJson(ErrorResponse::new(
                            "Failed to record usage".to_string(),
                            "server_error".to_string(),
                        )),
                    )
                        .into_response();
                }
            };

            let inference_id = uuid::Uuid::new_v4();
            let usage_request = services::usage::RecordUsageServiceRequest {
                organization_id,
                workspace_id,
                api_key_id,
                model_id,
                input_tokens: duration_seconds, // Bill by duration in seconds
                output_tokens: 0,
                inference_type: "audio_transcription".to_string(),
                ttft_ms: None,
                avg_itl_ms: None,
                inference_id: Some(inference_id),
                provider_request_id: None,
                stop_reason: Some(services::usage::StopReason::Completed),
                response_id: None,
                image_count: None,
            };

            // Record usage synchronously
            if let Err(e) = app_state.usage_service.record_usage(usage_request).await {
                tracing::error!(error = %e, "Failed to record audio transcription usage");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to record usage - please retry".to_string(),
                        "server_error".to_string(),
                    )),
                )
                    .into_response();
            }

            (StatusCode::OK, ResponseJson(response)).into_response()
        }
        Err(e) => {
            let (status_code, error_type, message) = match e {
                inference_providers::AudioTranscriptionError::TranscriptionError(msg) => {
                    tracing::error!(error = %msg, "Audio transcription provider error");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "Audio transcription failed".to_string(),
                    )
                }
                inference_providers::AudioTranscriptionError::HttpError {
                    status_code,
                    message,
                } => {
                    tracing::error!(status_code = status_code, error = %message, "Audio transcription HTTP error");
                    let code = StatusCode::from_u16(status_code)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    let msg = match code {
                        StatusCode::NOT_FOUND => "Model not found".to_string(),
                        StatusCode::BAD_REQUEST => "Invalid request".to_string(),
                        StatusCode::TOO_MANY_REQUESTS => "Rate limit exceeded".to_string(),
                        _ => "Audio transcription failed".to_string(),
                    };
                    (code, "server_error", msg)
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
