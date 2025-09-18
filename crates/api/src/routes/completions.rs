use crate::{
    conversions::{current_unix_timestamp, generate_completion_id},
    middleware::AuthenticatedUser,
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
use services::completions::ports::{
    CompletionMessage, CompletionRequest as ServiceCompletionRequest,
};
use std::convert::Infallible;
use tracing::debug;
use uuid::Uuid;

// Convert HTTP ChatCompletionRequest to service CompletionRequest
fn convert_chat_request_to_service(
    request: &ChatCompletionRequest,
    user_id: Uuid,
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
        metadata: None,
    }
}

// Convert HTTP CompletionRequest to service CompletionRequest
fn convert_text_request_to_service(
    request: &CompletionRequest,
    user_id: Uuid,
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
        metadata: None,
    }
}

pub async fn chat_completions(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(request): Json<ChatCompletionRequest>,
) -> axum::response::Response {
    debug!("Chat completions request from user: {}", user.0.id);
    debug!(
        "Request model: {}, stream: {:?}, messages: {}",
        request.model,
        request.stream,
        request.messages.len()
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
    let service_request = convert_chat_request_to_service(&request, user.0.id);

    // Call the completion service - it only supports streaming
    match app_state
        .completion_service
        .create_completion_stream(service_request)
        .await
    {
        Ok(stream) => {
            // Check if streaming is requested
            if request.stream == Some(true) {
                // Handle streaming response - convert CompletionStreamEvent to SSE
                let model = request.model.clone();
                let id = format!("chatcmpl-{}", generate_completion_id());
                let created = current_unix_timestamp();

                // Convert CompletionStreamEvent to SSE events
                let sse_stream = stream
                    .map(move |event| {
                        // Convert CompletionStreamEvent to OpenAI-compatible SSE format
                        match event.event_name.as_str() {
                            "completion.delta" => {
                                // Extract delta from event data
                                if let Some(delta_text) =
                                    event.data.get("delta").and_then(|d| d.as_str())
                                {
                                    let sse_chunk = StreamChunkResponse {
                                        id: id.clone(),
                                        object: "chat.completion.chunk".to_string(),
                                        created,
                                        model: model.clone(),
                                        choices: vec![StreamChoice {
                                            index: 0,
                                            delta: Delta {
                                                role: None,
                                                content: Some(delta_text.to_string()),
                                            },
                                            finish_reason: None,
                                        }],
                                        usage: None,
                                    };

                                    let json =
                                        serde_json::to_string(&sse_chunk).unwrap_or_default();
                                    Some(Ok::<_, Infallible>(Event::default().data(json)))
                                } else {
                                    None
                                }
                            }
                            "completion.completed" => {
                                // Final chunk with usage information
                                if let Some(usage_data) = event.data.get("usage") {
                                    let usage = if let Ok(usage_obj) =
                                        serde_json::from_value::<serde_json::Value>(
                                            usage_data.clone(),
                                        ) {
                                        Some(Usage {
                                            input_tokens: usage_obj
                                                .get("prompt_tokens")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0)
                                                as u32,
                                            input_tokens_details: Some(InputTokensDetails {
                                                cached_tokens: 0,
                                            }),
                                            output_tokens: usage_obj
                                                .get("completion_tokens")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0)
                                                as u32,
                                            output_tokens_details: Some(OutputTokensDetails {
                                                reasoning_tokens: 0,
                                            }),
                                            total_tokens: usage_obj
                                                .get("total_tokens")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0)
                                                as u32,
                                        })
                                    } else {
                                        None
                                    };

                                    let sse_chunk = StreamChunkResponse {
                                        id: id.clone(),
                                        object: "chat.completion.chunk".to_string(),
                                        created,
                                        model: model.clone(),
                                        choices: vec![StreamChoice {
                                            index: 0,
                                            delta: Delta {
                                                role: None,
                                                content: None,
                                            },
                                            finish_reason: Some("stop".to_string()),
                                        }],
                                        usage,
                                    };

                                    let json =
                                        serde_json::to_string(&sse_chunk).unwrap_or_default();
                                    Some(Ok::<_, Infallible>(Event::default().data(json)))
                                } else {
                                    None
                                }
                            }
                            "completion.error" => {
                                // Log error and skip
                                if let Some(error_msg) =
                                    event.data.get("error").and_then(|e| e.as_str())
                                {
                                    tracing::error!("Completion stream error: {}", error_msg);
                                }
                                None
                            }
                            _ => {
                                // Skip other event types (started, progress, etc.)
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

                // Collect all events from the stream
                let mut stream = Box::pin(stream);
                while let Some(event) = stream.next().await {
                    match event.event_name.as_str() {
                        "completion.delta" => {
                            if let Some(delta_text) =
                                event.data.get("delta").and_then(|d| d.as_str())
                            {
                                content.push_str(delta_text);
                            }
                        }
                        "completion.completed" => {
                            if let Some(usage_data) = event.data.get("usage") {
                                if let Ok(usage_obj) =
                                    serde_json::from_value::<serde_json::Value>(usage_data.clone())
                                {
                                    usage = Some(Usage {
                                        input_tokens: usage_obj
                                            .get("prompt_tokens")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0)
                                            as u32,
                                        input_tokens_details: Some(InputTokensDetails {
                                            cached_tokens: 0,
                                        }),
                                        output_tokens: usage_obj
                                            .get("completion_tokens")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0)
                                            as u32,
                                        output_tokens_details: Some(OutputTokensDetails {
                                            reasoning_tokens: 0,
                                        }),
                                        total_tokens: usage_obj
                                            .get("total_tokens")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0)
                                            as u32,
                                    });
                                }
                            }
                        }
                        "completion.error" => {
                            if let Some(error_msg) =
                                event.data.get("error").and_then(|e| e.as_str())
                            {
                                return (
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    ResponseJson(ErrorResponse::new(
                                        error_msg.to_string(),
                                        "completion_error".to_string(),
                                    )),
                                )
                                    .into_response();
                            }
                        }
                        _ => {
                            // Skip other events
                        }
                    }
                }

                // Build the complete response
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
                    usage: usage.unwrap_or_else(|| Usage::new(0, 0)),
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

pub async fn completions(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(request): Json<CompletionRequest>,
) -> axum::response::Response {
    debug!("Text completions request from user: {}", user.0.id);
    debug!(
        "Request model: {}, stream: {:?}, prompt length: {} chars",
        request.model,
        request.stream,
        request.prompt.len()
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
    let service_request = convert_text_request_to_service(&request, user.0.id);

    // Call the completion service - it only supports streaming
    match app_state
        .completion_service
        .create_completion_stream(service_request)
        .await
    {
        Ok(stream) => {
            // Check if streaming is requested
            if request.stream == Some(true) {
                // Handle streaming response - convert CompletionStreamEvent to SSE
                let model = request.model.clone();
                let id = format!("cmpl-{}", generate_completion_id());
                let created = current_unix_timestamp();

                // Convert CompletionStreamEvent to SSE events (same logic as chat completions)
                let sse_stream = stream
                    .map(move |event| {
                        // Convert CompletionStreamEvent to OpenAI-compatible SSE format for text completion
                        match event.event_name.as_str() {
                            "completion.delta" => {
                                // Extract delta from event data
                                if let Some(delta_text) =
                                    event.data.get("delta").and_then(|d| d.as_str())
                                {
                                    let sse_chunk = StreamChunkResponse {
                                        id: id.clone(),
                                        object: "text_completion".to_string(),
                                        created,
                                        model: model.clone(),
                                        choices: vec![StreamChoice {
                                            index: 0,
                                            delta: Delta {
                                                role: None,
                                                content: Some(delta_text.to_string()),
                                            },
                                            finish_reason: None,
                                        }],
                                        usage: None,
                                    };

                                    let json =
                                        serde_json::to_string(&sse_chunk).unwrap_or_default();
                                    Some(Ok::<_, Infallible>(Event::default().data(json)))
                                } else {
                                    None
                                }
                            }
                            "completion.completed" => {
                                // Final chunk with usage information
                                if let Some(usage_data) = event.data.get("usage") {
                                    let usage = if let Ok(usage_obj) =
                                        serde_json::from_value::<serde_json::Value>(
                                            usage_data.clone(),
                                        ) {
                                        Some(Usage {
                                            input_tokens: usage_obj
                                                .get("prompt_tokens")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0)
                                                as u32,
                                            input_tokens_details: Some(InputTokensDetails {
                                                cached_tokens: 0,
                                            }),
                                            output_tokens: usage_obj
                                                .get("completion_tokens")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0)
                                                as u32,
                                            output_tokens_details: Some(OutputTokensDetails {
                                                reasoning_tokens: 0,
                                            }),
                                            total_tokens: usage_obj
                                                .get("total_tokens")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0)
                                                as u32,
                                        })
                                    } else {
                                        None
                                    };

                                    let sse_chunk = StreamChunkResponse {
                                        id: id.clone(),
                                        object: "text_completion".to_string(),
                                        created,
                                        model: model.clone(),
                                        choices: vec![StreamChoice {
                                            index: 0,
                                            delta: Delta {
                                                role: None,
                                                content: None,
                                            },
                                            finish_reason: Some("stop".to_string()),
                                        }],
                                        usage,
                                    };

                                    let json =
                                        serde_json::to_string(&sse_chunk).unwrap_or_default();
                                    Some(Ok::<_, Infallible>(Event::default().data(json)))
                                } else {
                                    None
                                }
                            }
                            "completion.error" => {
                                // Log error and skip
                                if let Some(error_msg) =
                                    event.data.get("error").and_then(|e| e.as_str())
                                {
                                    tracing::error!("Completion stream error: {}", error_msg);
                                }
                                None
                            }
                            _ => {
                                // Skip other event types (started, progress, etc.)
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

                // Collect all events from the stream
                let mut stream = Box::pin(stream);
                while let Some(event) = stream.next().await {
                    match event.event_name.as_str() {
                        "completion.delta" => {
                            if let Some(delta_text) =
                                event.data.get("delta").and_then(|d| d.as_str())
                            {
                                content.push_str(delta_text);
                            }
                        }
                        "completion.completed" => {
                            if let Some(usage_data) = event.data.get("usage") {
                                if let Ok(usage_obj) =
                                    serde_json::from_value::<serde_json::Value>(usage_data.clone())
                                {
                                    usage = Some(Usage {
                                        input_tokens: usage_obj
                                            .get("prompt_tokens")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0)
                                            as u32,
                                        input_tokens_details: Some(InputTokensDetails {
                                            cached_tokens: 0,
                                        }),
                                        output_tokens: usage_obj
                                            .get("completion_tokens")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0)
                                            as u32,
                                        output_tokens_details: Some(OutputTokensDetails {
                                            reasoning_tokens: 0,
                                        }),
                                        total_tokens: usage_obj
                                            .get("total_tokens")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0)
                                            as u32,
                                    });
                                }
                            }
                        }
                        "completion.error" => {
                            if let Some(error_msg) =
                                event.data.get("error").and_then(|e| e.as_str())
                            {
                                return (
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    ResponseJson(ErrorResponse::new(
                                        error_msg.to_string(),
                                        "completion_error".to_string(),
                                    )),
                                )
                                    .into_response();
                            }
                        }
                        _ => {
                            // Skip other events
                        }
                    }
                }

                // Build the complete response
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
                    usage: usage.unwrap_or_else(|| Usage::new(0, 0)),
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

pub async fn models(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<ResponseJson<ModelsResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("Models list request from user: {}", user.0.id);

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

pub async fn quote(
    State(_app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<ResponseJson<QuoteResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("TDX quote request from user: {}", user.0.id);
    debug!("Admin endpoint accessed successfully");
    // TODO: Move quote endpoint to appropriate service (this doesn't belong in completions)
    // For now, return a not implemented error
    Err((
        StatusCode::NOT_IMPLEMENTED,
        ResponseJson(ErrorResponse::new(
            "Quote endpoint not implemented in completions service".to_string(),
            "not_implemented".to_string(),
        )),
    ))
}
