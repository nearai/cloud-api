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
use services::completions::ports::{
    CompletionMessage, CompletionRequest as ServiceCompletionRequest,
};
use std::convert::Infallible;
use std::sync::Arc;
use tracing::debug;
use utoipa;
use uuid::Uuid;

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

    // Clone request_hash before moving body_hash
    let request_hash = body_hash.hash.clone();

    // Convert HTTP request to service parameters
    // Note: Names are not passed - high-cardinality data is tracked via database, not metrics
    let service_request = convert_chat_request_to_service(
        &request,
        api_key.api_key.created_by_user_id.0,
        api_key.api_key.id.0.clone(),
        api_key.organization.id.0,
        api_key.workspace.id.0,
        body_hash,
    );

    // Check if streaming is requested
    if request.stream == Some(true) {
        let inference_provider_pool = app_state.inference_provider_pool.clone();
        let attestation_service = app_state.attestation_service.clone();

        // Call the streaming completion service
        match app_state
            .completion_service
            .create_chat_completion_stream(service_request)
            .await
        {
            Ok(stream) => {
                // Accumulate all SSE bytes for response hash computation
                let accumulated_bytes = Arc::new(tokio::sync::Mutex::new(Vec::new()));
                let chat_id_state = Arc::new(tokio::sync::Mutex::new(None::<String>));

                let accumulated_clone = accumulated_bytes.clone();
                let chat_id_clone = chat_id_state.clone();

                // Convert to raw bytes stream with proper SSE formatting
                let pool_clone = inference_provider_pool.clone();
                let req_hash_clone = request_hash.clone();
                let pool_clone2 = pool_clone.clone();
                let req_hash_clone2 = req_hash_clone.clone();
                let attestation_clone = attestation_service.clone();
                let byte_stream = stream
                    .then(move |result| {
                        let accumulated_inner = accumulated_clone.clone();
                        let chat_id_inner = chat_id_clone.clone();
                        let pool_inner = pool_clone.clone();
                        let req_hash_inner = req_hash_clone.clone();
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
                                                    let id_clone = id.clone();
                                                    let mut cid = chat_id_inner.lock().await;
                                                    if cid.is_none() {
                                                        *cid = Some(id_clone.clone());
                                                        // Register request hash immediately (response hash will be registered later)
                                                        // This ensures InterceptStream can find the hashes when it stores the signature
                                                        let pool_reg = pool_inner.clone();
                                                        let req_hash_reg = req_hash_inner.clone();
                                                        tokio::spawn(async move {
                                                            // Register with empty response hash for now, will update later
                                                            pool_reg
                                                                .register_signature_hashes_for_chat(
                                                                    &id_clone,
                                                                    req_hash_reg,
                                                                    "pending".to_string(),
                                                                )
                                                                .await;
                                                        });
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

                        // Compute response hash from accumulated bytes
                        let bytes_accumulated = accumulated_bytes.lock().await.clone();
                        let response_hash = {
                            use sha2::{Digest, Sha256};
                            let mut hasher = Sha256::new();
                            hasher.update(&bytes_accumulated);
                            format!("{:x}", hasher.finalize())
                        };

                        // Update response hash in InferenceProviderPool and database
                        if let Some(chat_id) = chat_id_state.lock().await.clone() {
                            let pool_final = pool_clone2.clone();
                            let req_hash_final = req_hash_clone2.clone();
                            let resp_hash_final = response_hash.clone();
                            let attestation_final = attestation_clone.clone();

                            // Update the hashes in the pool
                            pool_final
                                .register_signature_hashes_for_chat(
                                    &chat_id,
                                    req_hash_final.clone(),
                                    resp_hash_final.clone(),
                                )
                                .await;

                            // Update the signature in the database using store_response_signature
                            // This ensures the correct signature overwrites any "pending" signature stored by InterceptStream
                            // The database has ON CONFLICT DO UPDATE, so this will update the existing signature
                            let chat_id_for_update = chat_id.clone();
                            tokio::spawn(async move {
                                if let Err(e) = attestation_final.store_response_signature(
                                    &chat_id_for_update,
                                    req_hash_final,
                                    resp_hash_final,
                                ).await {
                                    tracing::error!("Failed to update signature with real response hash: {}", e);
                                } else {
                                    tracing::debug!("Successfully updated signature with real response hash for chat_id: {}", chat_id_for_update);
                                }
                            });
                        }

                        Ok::<Bytes, Infallible>(done_bytes)
                    }));

                // Return raw streaming response with SSE headers
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header(header::CONNECTION, "keep-alive")
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
                // Return the exact bytes from the provider for hash verification
                // This ensures clients can hash the response and compare with attestation endpoints
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/json")
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
        "Request model: {}, stream: {:?}, prompt length: {} chars, org: {}, workspace: {}",
        request.model,
        request.stream,
        request.prompt.len(),
        api_key.organization.id,
        api_key.workspace.id.0
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
            .map(|model| ModelInfo {
                id: model.model_name,
                object: "model".to_string(),
                created: 0,
                owned_by: "system".to_string(),
            })
            .collect(),
    };
    Ok(ResponseJson(response))
}
