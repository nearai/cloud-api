use crate::{
    middleware::{auth::AuthenticatedApiKey, RequestBodyHash},
    models::*,
    routes::{api::AppState, common::map_domain_error_to_status, files::MAX_FILE_SIZE},
};
use axum::{
    body::{Body, Bytes},
    extract::{Extension, Json, Multipart, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json as ResponseJson, Response},
};
use base64::Engine;
use futures::stream::StreamExt;
use services::common::encryption_headers as service_encryption_headers;
use services::completions::{
    hash_inference_id_to_uuid,
    ports::{CompletionMessage, CompletionRequest as ServiceCompletionRequest},
};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;
use utoipa;
use uuid::Uuid;

// Timeout for synchronous usage recording before response is returned
const USAGE_RECORDING_TIMEOUT_SECS: u64 = 5;

// Custom header for exposing the inference ID as a UUID
const HEADER_INFERENCE_ID: &str = "Inference-Id";

// Helper function to provide detailed error context for multipart parsing failures
fn analyze_multipart_error(e: &axum::extract::multipart::MultipartError) -> (StatusCode, String) {
    let error_str = e.to_string();

    // Match against known multipart error patterns to provide specific guidance
    let (status, message) = if error_str.contains("boundary") {
        (
            StatusCode::BAD_REQUEST,
            "Invalid Content-Type boundary in multipart request. Ensure the boundary parameter in the Content-Type header matches the actual boundary markers in the message body.".to_string(),
        )
    } else if error_str.contains("unexpected end") || error_str.contains("unexpected EOF") {
        (
            StatusCode::BAD_REQUEST,
            "Request body ended unexpectedly. The multipart message may be truncated or missing the final boundary marker.".to_string(),
        )
    } else if error_str.contains("field") && error_str.contains("size") {
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            "Individual form field size exceeded. A single field in the multipart request is too large.".to_string(),
        )
    } else if error_str.contains("field") {
        (
            StatusCode::BAD_REQUEST,
            "Invalid form field in multipart request. Check field encoding and format.".to_string(),
        )
    } else if error_str.contains("content") {
        (
            StatusCode::BAD_REQUEST,
            "Invalid Content-Type or content encoding in multipart request.".to_string(),
        )
    } else {
        (
            StatusCode::BAD_REQUEST,
            "Invalid multipart form data. The request does not conform to multipart/form-data format.".to_string(),
        )
    };

    (status, message)
}

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

// Helper function to check if model has vision capability (input_modalities contains "image")
fn has_vision_capability(input_modalities: &Option<Vec<String>>) -> bool {
    input_modalities
        .as_ref()
        .is_some_and(|modalities| modalities.contains(&"image".to_string()))
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
                tool_call_id: None,
                tool_calls: None,
                multimodal_content: None,
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
        store: None,
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
            tool_call_id: None,
            tool_calls: None,
            multimodal_content: None,
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
        store: None,
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
        "Request model: {}, stream: {:?}, org: {}, workspace: {}",
        request.model, request.stream, api_key.organization.id, api_key.workspace.id.0
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
                inference_type: services::usage::ports::InferenceType::ImageGeneration,
                ttft_ms: None,
                avg_itl_ms: None,
                inference_id,
                provider_request_id: Some(provider_request_id.clone()),
                stop_reason: Some(services::usage::StopReason::Completed),
                response_id: None,
                image_count: Some(image_count),
            };

            // Attempt synchronous usage recording with timeout before returning response.
            // This ensures usage is persisted to the database before the client considers the request complete.
            // If recording times out (>5s), fall back to fire-and-forget async task with retry logic.
            let timeout_duration = Duration::from_secs(USAGE_RECORDING_TIMEOUT_SECS);
            match tokio::time::timeout(
                timeout_duration,
                usage_service.record_usage(usage_request.clone()),
            )
            .await
            {
                Ok(Ok(())) => {
                    // Usage recorded successfully before response returned
                    tracing::debug!(
                        image_id = %provider_request_id,
                        %organization_id,
                        "Image generation usage recorded synchronously"
                    );
                }
                Ok(Err(e)) => {
                    // Recording failed, fall back to async retry
                    tracing::warn!(
                        error = %e,
                        image_id = %provider_request_id,
                        "Failed to record usage synchronously, retrying async"
                    );
                    let usage_service_clone = usage_service.clone();
                    tokio::spawn(async move {
                        if let Err(e) = usage_service_clone.record_usage(usage_request).await {
                            tracing::error!(
                                error = %e,
                                image_id = %provider_request_id,
                                "Failed to record image generation usage in async retry"
                            );
                        }
                    });
                }
                Err(_timeout) => {
                    // Recording timed out, fall back to async retry to avoid blocking client
                    tracing::warn!(
                        image_id = %provider_request_id,
                        "Usage recording timed out ({USAGE_RECORDING_TIMEOUT_SECS}s), retrying async"
                    );
                    let usage_service_clone = usage_service.clone();
                    tokio::spawn(async move {
                        if let Err(e) = usage_service_clone.record_usage(usage_request).await {
                            tracing::error!(
                                error = %e,
                                image_id = %provider_request_id,
                                "Failed to record image generation usage in async retry after timeout"
                            );
                        }
                    });
                }
            }

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
                organization_id,
                workspace_id,
                api_key_id,
                model_id,
                input_tokens: duration_seconds, // Bill by duration in seconds
                output_tokens: 0,
                inference_type: services::usage::ports::InferenceType::AudioTranscription,
                ttft_ms: None,
                avg_itl_ms: None,
                inference_id: Some(inference_id),
                provider_request_id: None,
                stop_reason: Some(services::usage::StopReason::Completed),
                response_id: None,
                image_count: None,
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

            (StatusCode::OK, ResponseJson(response)).into_response()
        }
        Err(e) => {
            let (status_code, error_type, message) = match e {
                services::completions::ports::CompletionError::RateLimitExceeded => {
                    tracing::warn!("Concurrent request limit exceeded for audio transcription");
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        "rate_limit_error",
                        "Too many concurrent audio transcription requests. Organization limit: 64 concurrent requests per model.".to_string(),
                    )
                }
                services::completions::ports::CompletionError::ProviderError(_) => {
                    // Don't log error details - may contain customer data
                    tracing::error!("Audio transcription provider error");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "Audio transcription failed".to_string(),
                    )
                }
                _ => {
                    // Don't log error details - may contain customer data
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
                inference_type: services::usage::ports::InferenceType::ImageEdit,
                ttft_ms: None,
                avg_itl_ms: None,
                inference_id,
                provider_request_id: Some(provider_request_id.clone()),
                stop_reason: Some(services::usage::StopReason::Completed),
                response_id: None,
                image_count: Some(image_count),
            };

            // Attempt synchronous usage recording with timeout before returning response.
            // This ensures usage is persisted to the database before the client considers the request complete.
            // If recording times out (>5s), fall back to fire-and-forget async task with retry logic.
            let timeout_duration = Duration::from_secs(USAGE_RECORDING_TIMEOUT_SECS);
            match tokio::time::timeout(
                timeout_duration,
                usage_service.record_usage(usage_request.clone()),
            )
            .await
            {
                Ok(Ok(())) => {
                    // Usage recorded successfully before response returned
                    tracing::debug!(
                        image_id = %provider_request_id,
                        %organization_id,
                        "Image edit usage recorded synchronously"
                    );
                }
                Ok(Err(e)) => {
                    // Recording failed, fall back to async retry
                    tracing::warn!(
                        error = %e,
                        image_id = %provider_request_id,
                        "Failed to record usage synchronously, retrying async"
                    );
                    let usage_service_clone = usage_service.clone();
                    tokio::spawn(async move {
                        if let Err(e) = usage_service_clone.record_usage(usage_request).await {
                            tracing::error!(
                                error = %e,
                                image_id = %provider_request_id,
                                "Failed to record image edit usage in async retry"
                            );
                        }
                    });
                }
                Err(_timeout) => {
                    // Recording timed out, fall back to async retry to avoid blocking client
                    tracing::warn!(
                        image_id = %provider_request_id,
                        "Usage recording timed out ({USAGE_RECORDING_TIMEOUT_SECS}s), retrying async"
                    );
                    let usage_service_clone = usage_service.clone();
                    tokio::spawn(async move {
                        if let Err(e) = usage_service_clone.record_usage(usage_request).await {
                            tracing::error!(
                                error = %e,
                                image_id = %provider_request_id,
                                "Failed to record image edit usage in async retry after timeout"
                            );
                        }
                    });
                }
            }

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

/// Helper function to perform image analysis
/// Shared logic between JSON and multipart endpoints
async fn perform_image_analysis(
    app_state: &AppState,
    api_key: &AuthenticatedApiKey,
    body_hash: String,
    headers: &axum::http::HeaderMap,
    model_name: String,
    prompt: String,
    image_url: String,
    max_tokens: Option<i32>,
    temperature: Option<f32>,
) -> axum::response::Response {
    // Log request (IDs only, per CLAUDE.md privacy rules)
    tracing::info!(
        model = %model_name,
        org_id = %api_key.organization.id,
        workspace_id = %api_key.workspace.id,
        "Image analysis request"
    );

    // Resolve model and validate it's a vision model
    let model = match app_state.models_service.get_model_by_name(&model_name).await {
        Ok(model) => model,
        Err(services::models::ModelsError::NotFound(_)) => {
            tracing::warn!(model = %model_name, "Model not found for image analysis");
            return (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    format!("Model '{}' not found", model_name),
                    "invalid_request_error".to_string(),
                )),
            )
            .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to resolve model for image analysis");
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

    // Validate model has vision capability
    if !has_vision_capability(&model.input_modalities) {
        tracing::warn!(
            model = %model_name,
            input_modalities = ?model.input_modalities,
            "Model does not support image analysis"
        );
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                format!("Model '{}' does not support image analysis. Model must have input_modalities including 'image'.", model_name),
                "invalid_request_error".to_string(),
            )),
        )
        .into_response();
    }

    // Build multimodal message content (OpenAI format)
    let message_content = serde_json::json!([
        {
            "type": "text",
            "text": prompt
        },
        {
            "type": "image_url",
            "image_url": {
                "url": image_url
            }
        }
    ]);

    let messages = vec![inference_providers::ChatMessage {
        role: inference_providers::MessageRole::User,
        content: Some(message_content),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }];

    // Extract encryption headers if present (for TEE attestation)
    let encryption_headers = match crate::routes::common::validate_encryption_headers(headers) {
        Ok(headers) => headers,
        Err(err) => return err.into_response(),
    };

    let mut extra = std::collections::HashMap::new();
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

    let params = inference_providers::ChatCompletionParams {
        model: model_name,
        messages,
        max_completion_tokens: max_tokens.map(|t| t as i64),
        temperature,
        stream: Some(false), // For MVP, only non-streaming
        extra,
        max_tokens: None,
        top_p: None,
        n: None,
        stop: None,
        presence_penalty: None,
        frequency_penalty: None,
        logit_bias: None,
        logprobs: None,
        top_logprobs: None,
        user: None,
        seed: None,
        tools: None,
        tool_choice: None,
        parallel_tool_calls: None,
        metadata: None,
        store: None,
        stream_options: None,
        modalities: None,
    };

    // Call inference provider (non-streaming for MVP)
    match app_state
        .inference_provider_pool
        .chat_completion(params, body_hash)
        .await
    {
        Ok(response) => {
            // Extract analysis text from first choice
            let analysis = response
                .response
                .choices
                .first()
                .and_then(|c| c.message.content.as_ref())
                .cloned()
                .unwrap_or_default();

            let usage = crate::models::ImageAnalysisUsage {
                prompt_tokens: response.response.usage.prompt_tokens,
                completion_tokens: response.response.usage.completion_tokens,
                total_tokens: response.response.usage.total_tokens,
            };

            let analysis_response = crate::models::ImageAnalysisResponse {
                id: response.response.id,
                object: "image.analysis".to_string(),
                created: response.response.created,
                model: model.model_name,
                analysis,
                usage: Some(usage),
            };

            tracing::info!("Image analysis completed successfully");
            (StatusCode::OK, ResponseJson(analysis_response)).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "Image analysis inference failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Image analysis failed".to_string(),
                    "server_error".to_string(),
                )),
            )
            .into_response()
        }
    }
}

/// Document reranking endpoint
///
/// Ranks documents by relevance to a query using a reranker model.
///
/// **Concurrent Request Limits:** Each organization has a per-model concurrent request limit (default: 64).
/// When the limit is reached, new requests will fail with 429 status code. Wait for in-flight requests to complete before retrying.
#[utoipa::path(
    post,
    path = "/v1/rerank",
    tag = "Rerank",
    request_body = crate::models::RerankRequest,
    responses(
        (status = 200, description = "Successful rerank", body = crate::models::RerankResponse),
        (status = 400, description = "Invalid request (empty documents, invalid model, etc.)", body = ErrorResponse),
        (status = 401, description = "Unauthorized (missing or invalid API key)", body = ErrorResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 429, description = "Concurrent request limit exceeded for the organization. Max concurrent requests per model: 64 (configurable)", body = ErrorResponse),
        (status = 500, description = "Server error (billing failure, provider error)", body = ErrorResponse),
    ),
    security(("ApiKeyAuth" = []))
)]
pub async fn rerank(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(_body_hash): Extension<RequestBodyHash>,
    Json(request): Json<crate::models::RerankRequest>,
) -> axum::response::Response {
    debug!(
        "Rerank request: model={}, org={}, workspace={}",
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
            tracing::error!(error = %e, "Failed to resolve model for rerank");
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

    // Convert API request to provider params
    let params = inference_providers::RerankParams {
        model: request.model.clone(),
        query: request.query.clone(),
        documents: request.documents.clone(),
        extra: std::collections::HashMap::new(),
    };

    // Call completion service which handles concurrent request limiting
    // Each organization has a per-model concurrent request limit (default: 64 concurrent requests).
    // This prevents resource exhaustion and ensures fair usage. Returns 429 if limit exceeded.
    match app_state
        .completion_service
        .try_rerank(organization_id, model_id, &request.model, params)
        .await
    {
        Ok(response) => {
            // Record usage for rerank SYNCHRONOUSLY to ensure billing accuracy
            // This is critical for revenue tracking and must complete before returning response
            let organization_id = api_key.organization.id.0;
            let workspace_id = api_key.workspace.id.0;
            let mut token_count = response
                .usage
                .as_ref()
                .and_then(|u| u.total_tokens)
                .unwrap_or(0);

            // Validate token count is reasonable (prevent provider misreporting)
            const MAX_REASONABLE_TOKENS: i32 = 1_000_000; // 1M tokens max
            let mut token_anomaly_detected = false;

            if token_count > MAX_REASONABLE_TOKENS {
                // Log at ERROR level with full context for monitoring/alerting
                // This indicates a provider bug that needs investigation
                tracing::error!(
                    token_count = token_count,
                    max_expected = MAX_REASONABLE_TOKENS,
                    model = %request.model,
                    organization_id = %organization_id,
                    "Provider returned unreasonable token count - capping to prevent billing errors. This may indicate provider misconfiguration or a bug."
                );
                token_anomaly_detected = true;

                // Record metrics for monitoring and alerting
                let model_tag = format!("model:{}", request.model);
                let reason_tag = format!(
                    "reason:{}",
                    services::metrics::consts::REASON_TOKEN_OVERFLOW
                );
                let anomaly_tags = [model_tag.as_str(), reason_tag.as_str()];
                app_state.metrics_service.record_count(
                    services::metrics::consts::METRIC_PROVIDER_TOKEN_ANOMALIES,
                    1,
                    &anomaly_tags,
                );

                // Cap to maximum to prevent billing errors
                token_count = MAX_REASONABLE_TOKENS;
            }

            // Warn if provider didn't return usage data
            if token_count == 0 {
                tracing::warn!(
                    model = %request.model,
                    organization_id = %organization_id,
                    "Provider returned zero tokens for rerank - no cost will be charged. This may indicate provider misconfiguration or incomplete response."
                );
                token_anomaly_detected = true;

                // Record metrics for monitoring and alerting on missing usage data
                let model_tag = format!("model:{}", request.model);
                let reason_tag =
                    format!("reason:{}", services::metrics::consts::REASON_MISSING_USAGE);
                let zero_tokens_tags = [model_tag.as_str(), reason_tag.as_str()];
                app_state.metrics_service.record_count(
                    services::metrics::consts::METRIC_PROVIDER_ZERO_TOKENS,
                    1,
                    &zero_tokens_tags,
                );
            }

            // If anomaly detected, log additional context for debugging
            if token_anomaly_detected {
                tracing::info!(
                    model = %request.model,
                    organization_id = %organization_id,
                    final_token_count = token_count,
                    "Token count anomaly: Provider data quality issue detected. Recommendation: Check provider logs and configuration."
                );
            }

            // Parse API key ID to UUID
            let api_key_id = match Uuid::parse_str(&api_key.api_key.id.0) {
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
                input_tokens: token_count,
                output_tokens: 0,
                inference_type: services::usage::ports::InferenceType::Rerank,
                ttft_ms: None,
                avg_itl_ms: None,
                inference_id: Some(inference_id),
                provider_request_id: None,
                stop_reason: Some(services::usage::StopReason::Completed),
                response_id: None,
                image_count: None,
            };

            // Record usage synchronously - this is billing-critical and must succeed
            // If usage recording fails, we must fail the request to prevent revenue loss
            if let Err(e) = app_state.usage_service.record_usage(usage_request).await {
                tracing::error!(error = %e, "Failed to record rerank usage - blocking request to ensure billing accuracy");
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
                services::completions::ports::CompletionError::RateLimitExceeded => {
                    tracing::warn!("Concurrent request limit exceeded for rerank");
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        "rate_limit_error",
                        "Too many concurrent rerank requests. Organization limit: 64 concurrent requests per model. Please wait for in-flight requests to complete before retrying.".to_string(),
                    )
                }
                services::completions::ports::CompletionError::ProviderError(msg) => {
                    tracing::error!(error = %msg, "Rerank provider error");
                    // Check if it's a model not found error
                    if msg.contains("not found") || msg.contains("does not exist") {
                        (
                            StatusCode::NOT_FOUND,
                            "not_found_error",
                            "Model not found".to_string(),
                        )
                    } else {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "server_error",
                            "Reranking failed".to_string(),
                        )
                    }
                }
                _ => {
                    tracing::error!(error = %e, "Unexpected rerank error");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "Reranking failed".to_string(),
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

/// Analyze an image and return text description (JSON with data URL or file ID)
///
/// Use a vision model to analyze an image and answer a question about it.
/// OpenAI-compatible endpoint for image analysis.
#[utoipa::path(
    post,
    path = "/v1/images/analyses",
    tag = "Images",
    request_body = ImageAnalysisRequest,
    responses(
        (status = 200, description = "Image analysis successful", body = ImageAnalysisResponse),
        (status = 400, description = "Invalid request parameters", body = ErrorResponse),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn image_analyses(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    headers: header::HeaderMap,
    Json(request): Json<crate::models::ImageAnalysisRequest>,
) -> axum::response::Response {
    // Validate request
    if let Err(error) = request.validate() {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(error, "invalid_request_error".to_string())),
        )
        .into_response();
    }

    // Convert image input to URL format
    let image_url = match &request.image {
        crate::models::ImageInput::DataUrl(data_url) => {
            // Validate data URL format
            if !data_url.starts_with("data:image/") {
                return (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        "Image must be a valid data URL starting with 'data:image/'".to_string(),
                        "invalid_request_error".to_string(),
                    )),
                )
                .into_response();
            }
            data_url.clone()
        }
        crate::models::ImageInput::FileId { file_id } => format!("/v1/files/{}", file_id),
    };

    perform_image_analysis(
        &app_state,
        &api_key,
        body_hash.hash.clone(),
        &headers,
        request.model,
        request.prompt,
        image_url,
        request.max_tokens,
        request.temperature,
    )
    .await
}

/// Analyze an image and return text description (multipart form data with file upload)
///
/// Use a vision model to analyze an image file and answer a question about it.
/// Accepts file uploads via multipart/form-data.
///
/// **Request Body (multipart/form-data):**
/// - `model` (text, required): Model ID (e.g., "Qwen/Qwen3-VL-30B-A3B-Instruct")
/// - `image` (file, required): Image file (PNG or JPEG)
/// - `prompt` (text, required): Question or description of what to analyze
/// - `max_tokens` (text, optional): Maximum tokens in response
/// - `temperature` (text, optional): Sampling temperature (0.0-2.0)
#[utoipa::path(
    post,
    path = "/v1/images/analyses/upload",
    tag = "Images",
    request_body(content = ImageEditRequestSchema, content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "Image analysis successful", body = ImageAnalysisResponse),
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
pub async fn image_analyses_multipart(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    headers: header::HeaderMap,
    mut multipart: axum::extract::Multipart,
) -> axum::response::Response {
    debug!("Image analysis request (multipart) from api key");

    let mut model = String::new();
    let mut prompt = String::new();
    let mut image_bytes: Option<Vec<u8>> = None;
    let mut max_tokens: Option<i32> = None;
    let mut temperature: Option<f32> = None;

    // Parse multipart form data
    loop {
        match multipart.next_field().await {
            Ok(Some(mut field)) => {
                match field.name().unwrap_or("") {
                    "image" => {
                        // Use streaming validation to detect oversized payloads before buffering
                        let mut bytes = Vec::new();
                        let mut size = 0usize;

                        loop {
                            match field.chunk().await {
                                Ok(Some(chunk)) => {
                                    size += chunk.len();
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

                        // Validate image format (PNG or JPEG)
                        let is_png = bytes.len() >= 4 && &bytes[0..4] == b"\x89PNG";
                        let is_jpeg = bytes.len() >= 3 && &bytes[0..3] == b"\xFF\xD8\xFF";
                        if !is_png && !is_jpeg {
                            return (
                                StatusCode::BAD_REQUEST,
                                ResponseJson(ErrorResponse::new(
                                    "Image must be a valid PNG or JPEG file".to_string(),
                                    "invalid_request_error".to_string(),
                                )),
                            )
                            .into_response();
                        }

                        image_bytes = Some(bytes);
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
                    "max_tokens" => match field.text().await {
                        Ok(text) => {
                            max_tokens = text.parse::<i32>().ok();
                        }
                        Err(e) => {
                            return (
                                StatusCode::BAD_REQUEST,
                                ResponseJson(ErrorResponse::new(
                                    format!("Failed to read max_tokens: {}", e),
                                    "invalid_request_error".to_string(),
                                )),
                            )
                            .into_response();
                        }
                    },
                    "temperature" => match field.text().await {
                        Ok(text) => {
                            temperature = text.parse::<f32>().ok();
                        }
                        Err(e) => {
                            return (
                                StatusCode::BAD_REQUEST,
                                ResponseJson(ErrorResponse::new(
                                    format!("Failed to read temperature: {}", e),
                                    "invalid_request_error".to_string(),
                                )),
                            )
                            .into_response();
                        }
                    },
                    _ => {} // Ignore unknown fields
                }
            }
            Ok(None) => break,
            Err(e) => {
                let (status, message) = analyze_multipart_error(&e);
                return (status, ResponseJson(ErrorResponse::new(message, "invalid_request_error".to_string())))
                    .into_response();
            }
        }
    }

    // Validate required fields
    if model.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "model is required".to_string(),
                "invalid_request_error".to_string(),
            )),
        )
        .into_response();
    }

    if prompt.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "prompt is required".to_string(),
                "invalid_request_error".to_string(),
            )),
        )
        .into_response();
    }

    let image_bytes = match image_bytes {
        Some(bytes) => bytes,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    "image is required".to_string(),
                    "invalid_request_error".to_string(),
                )),
            )
            .into_response();
        }
    };

    // Convert image bytes to base64 data URL
    let base64_image = base64::engine::general_purpose::STANDARD.encode(&image_bytes);
    let image_url = format!("data:image/png;base64,{}", base64_image);

    perform_image_analysis(
        &app_state,
        &api_key,
        body_hash.hash.clone(),
        &headers,
        model,
        prompt,
        image_url,
        max_tokens,
        temperature,
    )
    .await
}

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
        (status = 429, description = "Concurrent request limit exceeded for the organization. Max concurrent requests per model: 64 (configurable)", body = ErrorResponse),
        (status = 500, description = "Server error (billing failure, provider error)", body = ErrorResponse),
    ),
    security(("ApiKeyAuth" = []))
)]
pub async fn score(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    Json(request): Json<crate::models::ScoreRequest>,
) -> axum::response::Response {
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
            inference_providers::ScoreParams {
                model: request.model.clone(),
                text_1: request.text_1.clone(),
                text_2: request.text_2.clone(),
                extra: std::collections::HashMap::new(),
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
            let api_key_id = match Uuid::parse_str(&api_key.api_key.id.0) {
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

            let inference_id = hash_inference_id_to_uuid(&response.id);

            // Score requests don't have traditional token counts - use input token count as 1
            let usage_request = services::usage::ports::RecordUsageServiceRequest {
                organization_id,
                workspace_id,
                api_key_id,
                model_id,
                input_tokens: 1,
                output_tokens: 0,
                inference_type: services::usage::ports::InferenceType::Score,
                ttft_ms: None,
                avg_itl_ms: None,
                inference_id: Some(inference_id),
                provider_request_id: None,
                stop_reason: Some(services::usage::StopReason::Completed),
                response_id: None,
                image_count: None,
            };

            // Record usage with timeout to prevent blocking responses
            match tokio::time::timeout(
                Duration::from_secs(USAGE_RECORDING_TIMEOUT_SECS),
                app_state.usage_service.record_usage(usage_request),
            )
            .await
            {
                Ok(Ok(())) => ResponseJson(response).into_response(),
                Ok(Err(e)) => {
                    tracing::warn!(
                        error = %e,
                        model_id = %model_id,
                        "Failed to record usage synchronously, retrying async"
                    );
                    let usage_service_clone = app_state.usage_service.clone();
                    let usage_request_retry = services::usage::ports::RecordUsageServiceRequest {
                        organization_id,
                        workspace_id,
                        api_key_id,
                        model_id,
                        input_tokens: 1,
                        output_tokens: 0,
                        inference_type: services::usage::ports::InferenceType::Score,
                        ttft_ms: None,
                        avg_itl_ms: None,
                        inference_id: Some(inference_id),
                        provider_request_id: None,
                        stop_reason: Some(services::usage::StopReason::Completed),
                        response_id: None,
                        image_count: None,
                    };
                    tokio::spawn(async move {
                        if let Err(e) = usage_service_clone.record_usage(usage_request_retry).await
                        {
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
                    let usage_request_retry = services::usage::ports::RecordUsageServiceRequest {
                        organization_id,
                        workspace_id,
                        api_key_id,
                        model_id,
                        input_tokens: 1,
                        output_tokens: 0,
                        inference_type: services::usage::ports::InferenceType::Score,
                        ttft_ms: None,
                        avg_itl_ms: None,
                        inference_id: Some(inference_id),
                        provider_request_id: None,
                        stop_reason: Some(services::usage::StopReason::Completed),
                        response_id: None,
                        image_count: None,
                    };
                    tokio::spawn(async move {
                        if let Err(e) = usage_service_clone.record_usage(usage_request_retry).await
                        {
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
                services::completions::ports::CompletionError::RateLimitExceeded => {
                    tracing::warn!("Concurrent request limit exceeded for score");
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        "rate_limit_error",
                        "Too many concurrent score requests. Organization limit: 64 concurrent requests per model. Please wait for in-flight requests to complete before retrying.".to_string(),
                    )
                }
                services::completions::ports::CompletionError::ProviderError(msg) => {
                    tracing::error!(error = %msg, "Score provider error");
                    // Check if it's a model not found error
                    if msg.contains("not found") || msg.contains("does not exist") {
                        (
                            StatusCode::NOT_FOUND,
                            "not_found_error",
                            "Model not found".to_string(),
                        )
                    } else {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "server_error",
                            "Scoring failed".to_string(),
                        )
                    }
                }
                _ => {
                    tracing::error!(error = %e, "Unexpected score error");
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
