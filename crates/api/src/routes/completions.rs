use crate::{
    conversions::{current_unix_timestamp, generate_completion_id},
    middleware::auth::AuthenticatedApiKey,
    models::*,
    routes::{api::AppState, common::map_domain_error_to_status},
};
use axum::{
    extract::{Extension, Json, State},
    http::StatusCode,
    response::{
        sse::{Event, Sse},
        IntoResponse, Json as ResponseJson,
    },
};
use futures::stream::StreamExt;
use inference_providers::{FinishReason, StreamChunk};
use services::completions::ports::{
    CompletionMessage, CompletionRequest as ServiceCompletionRequest,
};
use std::convert::Infallible;
use tracing::debug;
use utoipa;
use uuid::Uuid;

// Convert HTTP ChatCompletionRequest to service CompletionRequest
fn convert_chat_request_to_service(
    request: &ChatCompletionRequest,
    user_id: Uuid,
    api_key_id: String,
    organization_id: Uuid,
    workspace_id: Uuid,
) -> ServiceCompletionRequest {
    ServiceCompletionRequest {
        model: request.model.clone(),
        messages: request
            .messages
            .iter()
            .map(|msg| CompletionMessage {
                role: msg.role.clone(),
                content: msg.content.clone(),
            })
            .collect(),
        max_tokens: request.max_tokens,
        temperature: request.temperature,
        top_p: request.top_p,
        stop: request.stop.clone(),
        stream: request.stream,
        user_id: user_id.into(),
        api_key_id,
        organization_id,
        workspace_id,
        metadata: None,
    }
}

// Convert HTTP CompletionRequest to service CompletionRequest
fn convert_text_request_to_service(
    request: &CompletionRequest,
    user_id: Uuid,
    api_key_id: String,
    organization_id: Uuid,
    workspace_id: Uuid,
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
        api_key_id,
        organization_id,
        workspace_id,
        metadata: None,
    }
}

/// Create a chat completion
///
/// Creates a completion for a given chat conversation.
#[utoipa::path(
    post,
    path = "/chat/completions",
    tag = "Chat",
    request_body = ChatCompletionRequest,
    responses(
        (status = 200, description = "Successful completion", body = ChatCompletionResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn chat_completions(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
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
    let service_request = convert_chat_request_to_service(
        &request,
        api_key.api_key.created_by_user_id.0,
        api_key.api_key.id.0.clone(),
        api_key.organization.id.0,
        api_key.workspace.id.0,
    );

    // Call the completion service - it handles usage tracking internally
    match app_state
        .completion_service
        .create_completion_stream(service_request)
        .await
    {
        Ok(stream) => {
            // Check if streaming is requested
            if request.stream == Some(true) {
                let sse_stream = stream
                    .map(|chunk| match chunk {
                        Ok(chunk) => Ok::<_, Infallible>(
                            Event::default()
                                .data(serde_json::to_string(&chunk).unwrap_or_default()),
                        ),
                        Err(e) => {
                            tracing::error!("Completion stream error: {}", e);
                            Ok::<_, Infallible>(
                                Event::default()
                                    .data(serde_json::to_string(&e).unwrap_or_default()),
                            )
                        }
                    })
                    .chain(futures::stream::once(async move {
                        Ok::<_, Infallible>(Event::default().data("[DONE]"))
                    }));

                // Return SSE response
                Sse::new(sse_stream)
                    .keep_alive(
                        axum::response::sse::KeepAlive::new()
                            .interval(std::time::Duration::from_secs(30))
                            .text("keep-alive-text"),
                    )
                    .into_response()
            } else {
                // Handle non-streaming response - collect the stream
                let mut content = String::new();
                let mut usage: Option<Usage> = None;

                // Pin the stream for iteration
                let mut stream_pin = stream;
                while let Some(chunk_result) = stream_pin.next().await {
                    match chunk_result {
                        Ok(StreamChunk::Chat(chunk)) => {
                            // Extract content from delta
                            if let Some(choice) = chunk.choices.first() {
                                if let Some(delta) = &choice.delta {
                                    if let Some(delta_content) = &delta.content {
                                        content.push_str(delta_content);
                                    }
                                }
                            }

                            // Extract usage from final chunk
                            if let Some(chunk_usage) = chunk.usage {
                                usage = Some(Usage {
                                    input_tokens: chunk_usage.prompt_tokens,
                                    input_tokens_details: Some(InputTokensDetails {
                                        cached_tokens: 0,
                                    }),
                                    output_tokens: chunk_usage.completion_tokens,
                                    output_tokens_details: Some(OutputTokensDetails {
                                        reasoning_tokens: 0,
                                    }),
                                    total_tokens: chunk_usage.total_tokens,
                                });
                            }
                        }
                        Ok(StreamChunk::Text(_)) => {
                            // Handle text completion if needed
                        }
                        Err(e) => {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                ResponseJson(ErrorResponse::new(
                                    e.to_string(),
                                    "completion_error".to_string(),
                                )),
                            )
                                .into_response();
                        }
                    }
                }

                // Build the complete response
                let final_usage = usage.unwrap_or_else(|| Usage::new(0, 0));
                let response = ChatCompletionResponse {
                    id: format!("chatcmpl-{}", generate_completion_id()),
                    object: "chat.completion".to_string(),
                    created: current_unix_timestamp(),
                    model: request.model.clone(),
                    choices: vec![ChatChoice {
                        index: 0,
                        message: Message {
                            role: "assistant".to_string(),
                            content,
                            name: None,
                        },
                        finish_reason: Some("stop".to_string()),
                    }],
                    usage: final_usage,
                };

                (StatusCode::OK, ResponseJson(response)).into_response()
            }
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

/// Create a text completion
///
/// Creates a completion for a given text prompt.
#[utoipa::path(
    post,
    path = "/completions",
    tag = "Chat",
    request_body = CompletionRequest,
    responses(
        (status = 200, description = "Successful completion", body = ChatCompletionResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn completions(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
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
    let service_request = convert_text_request_to_service(
        &request,
        api_key.api_key.created_by_user_id.0,
        api_key.api_key.id.0.clone(),
        api_key.organization.id.0,
        api_key.workspace.id.0,
    );

    // Call the completion service - it handles usage tracking internally
    match app_state
        .completion_service
        .create_completion_stream(service_request)
        .await
    {
        Ok(stream) => {
            // Check if streaming is requested
            if request.stream == Some(true) {
                // Convert raw StreamChunk to SSE events
                let sse_stream = stream
                    .map(move |chunk_result| {
                        match chunk_result {
                            Ok(StreamChunk::Text(chunk)) => {
                                // Convert to OpenAI-compatible SSE format
                                if let Some(choice) = chunk.choices.first() {
                                    let response = StreamChunkResponse {
                                        id: format!("cmpl-{}", generate_completion_id()),
                                        object: "text_completion".to_string(),
                                        created: current_unix_timestamp(),
                                        model: request.model.clone(),
                                        choices: vec![StreamChoice {
                                            index: 0,
                                            delta: Delta {
                                                role: None,
                                                content: Some(choice.text.clone()),
                                            },
                                            finish_reason: choice.finish_reason.as_ref().map(|f| {
                                                match f {
                                                    FinishReason::Stop => "stop".to_string(),
                                                    FinishReason::Length => "length".to_string(),
                                                    FinishReason::ContentFilter => {
                                                        "content_filter".to_string()
                                                    }
                                                }
                                            }),
                                        }],
                                        usage: None,
                                    };

                                    match serde_json::to_string(&response) {
                                        Ok(json) => {
                                            Some(Ok::<_, Infallible>(Event::default().data(json)))
                                        }
                                        Err(e) => {
                                            tracing::error!("JSON serialization error: {}", e);
                                            None
                                        }
                                    }
                                } else {
                                    None
                                }
                            }
                            Ok(StreamChunk::Chat(_)) => {
                                tracing::warn!("Received chat chunk in text completion endpoint");
                                None
                            }
                            Err(e) => {
                                tracing::error!("Completion stream error: {}", e);
                                None
                            }
                        }
                    })
                    .filter_map(|result| async move { result })
                    .chain(futures::stream::once(async move {
                        Ok::<_, Infallible>(Event::default().data("[DONE]"))
                    }));

                // Return SSE response
                Sse::new(sse_stream)
                    .keep_alive(
                        axum::response::sse::KeepAlive::new()
                            .interval(std::time::Duration::from_secs(30))
                            .text("keep-alive-text"),
                    )
                    .into_response()
            } else {
                // Handle non-streaming response - collect the stream
                let mut content = String::new();
                let mut usage: Option<Usage> = None;

                let mut stream_pin = stream;
                while let Some(chunk_result) = stream_pin.next().await {
                    match chunk_result {
                        Ok(StreamChunk::Chat(chunk)) => {
                            // Extract content from delta
                            if let Some(choice) = chunk.choices.first() {
                                if let Some(delta) = &choice.delta {
                                    if let Some(delta_content) = &delta.content {
                                        content.push_str(delta_content);
                                    }
                                }
                            }

                            // Extract usage from final chunk
                            if let Some(chunk_usage) = chunk.usage {
                                usage = Some(Usage {
                                    input_tokens: chunk_usage.prompt_tokens,
                                    input_tokens_details: Some(InputTokensDetails {
                                        cached_tokens: 0,
                                    }),
                                    output_tokens: chunk_usage.completion_tokens,
                                    output_tokens_details: Some(OutputTokensDetails {
                                        reasoning_tokens: 0,
                                    }),
                                    total_tokens: chunk_usage.total_tokens,
                                });
                            }
                        }
                        Ok(StreamChunk::Text(_)) => {
                            // Handle text completion if needed
                        }
                        Err(e) => {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                ResponseJson(ErrorResponse::new(
                                    e.to_string(),
                                    "completion_error".to_string(),
                                )),
                            )
                                .into_response();
                        }
                    }
                }

                // Build the complete response
                let final_usage = usage.unwrap_or_else(|| Usage::new(0, 0));
                let response = CompletionResponse {
                    id: format!("cmpl-{}", generate_completion_id()),
                    object: "text_completion".to_string(),
                    created: current_unix_timestamp(),
                    model: request.model.clone(),
                    choices: vec![CompletionChoice {
                        index: 0,
                        text: content,
                        logprobs: None,
                        finish_reason: Some("stop".to_string()),
                    }],
                    usage: final_usage,
                };

                (StatusCode::OK, ResponseJson(response)).into_response()
            }
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
/// Lists all available AI models that can be used for completions.
/// No pagination to follow the OpenAI API spec
#[utoipa::path(
    get,
    path = "/models",
    tag = "Models",
    responses(
        (status = 200, description = "List of available models", body = ModelsResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
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

    let models = app_state.models_service.get_models().await.map_err(|e| {
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
                id: model.id,
                object: model.object,
                created: model.created,
                owned_by: model.owned_by,
            })
            .collect(),
    };
    Ok(ResponseJson(response))
}
