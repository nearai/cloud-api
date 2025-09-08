use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::{Json as ResponseJson, sse::{Event, Sse}, IntoResponse},
};
use crate::{models::*, conversions::*};
use domain::{Domain, ChatCompletionParams, CompletionParams};
use std::sync::Arc;
use futures::stream::StreamExt;
use std::convert::Infallible;

fn map_domain_error_to_status(error: &domain::CompletionError) -> StatusCode {
    match error {
        domain::CompletionError::InvalidModel(_) | domain::CompletionError::InvalidParams(_) => StatusCode::BAD_REQUEST,
        domain::CompletionError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        domain::CompletionError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

// Application state containing the domain service
pub type AppState = Arc<Domain>;

pub async fn chat_completions(
    State(domain): State<AppState>,
    Json(request): Json<ChatCompletionRequest>,
) -> axum::response::Response {
    // Validate the request
    if let Err(error) = request.validate() {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(error, "invalid_request_error".to_string())),
        ).into_response();
    }

    // Convert HTTP request to domain parameters
    let domain_params: ChatCompletionParams = (&request).into();
    
    // Check if streaming is requested
    if request.stream == Some(true) {
        // Handle streaming response
        match domain.chat_completion_stream(domain_params).await {
            Ok(stream) => {
                let model = request.model.clone();
                let id = format!("chatcmpl-{}", generate_completion_id());
                let created = current_unix_timestamp();
                
                // Convert the domain stream to SSE events
                let sse_stream = stream
                    .map(move |chunk_result| {
                        match chunk_result {
                            Ok(chunk) => {
                                // Convert domain StreamChunk to SSE event
                                let sse_chunk = StreamChunkResponse {
                                    id: id.clone(),
                                    object: "chat.completion.chunk".to_string(),
                                    created,
                                    model: model.clone(),
                                    choices: chunk.choices.into_iter().map(|choice| StreamChoice {
                                        index: choice.index,
                                        delta: Delta {
                                            role: choice.delta.role,
                                            content: choice.delta.content,
                                        },
                                        finish_reason: choice.finish_reason,
                                    }).collect(),
                                    usage: chunk.usage.map(|u| Usage {
                                        prompt_tokens: u.prompt_tokens,
                                        completion_tokens: u.completion_tokens,
                                        total_tokens: u.total_tokens,
                                    }),
                                };
                                
                                let json = serde_json::to_string(&sse_chunk).unwrap_or_default();
                                Some(Ok::<_, Infallible>(Event::default().data(json)))
                            }
                            Err(e) => {
                                // Log error and skip this chunk
                                tracing::error!("Stream error: {:?}", e);
                                // Skip this chunk
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
            }
            Err(domain_error) => {
                let status_code = map_domain_error_to_status(&domain_error);
                (status_code, ResponseJson::<ErrorResponse>(domain_error.into())).into_response()
            }
        }
    } else {
        // Handle non-streaming response
        match domain.chat_completion(domain_params).await {
            Ok(result) => {
                let response = chat_completion_to_http_response(
                    result,
                    &request.model,
                    format!("chatcmpl-{}", generate_completion_id()),
                    current_unix_timestamp(),
                );
                (StatusCode::OK, ResponseJson(response)).into_response()
            }
            Err(domain_error) => {
                let status_code = map_domain_error_to_status(&domain_error);
                (status_code, ResponseJson::<ErrorResponse>(domain_error.into())).into_response()
            }
        }
    }
}

pub async fn completions(
    State(domain): State<AppState>,
    Json(request): Json<CompletionRequest>,
) -> axum::response::Response {
    // Validate the request
    if let Err(error) = request.validate() {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(error, "invalid_request_error".to_string())),
        ).into_response();
    }

    // Convert HTTP request to domain parameters
    let domain_params: CompletionParams = (&request).into();
    
    // Check if streaming is requested
    if request.stream == Some(true) {
        // Handle streaming response
        match domain.text_completion_stream(domain_params).await {
            Ok(stream) => {
                let model = request.model.clone();
                let id = format!("cmpl-{}", generate_completion_id());
                let created = current_unix_timestamp();
                
                // Convert the domain stream to SSE events
                let sse_stream = stream
                    .map(move |chunk_result| {
                        match chunk_result {
                            Ok(chunk) => {
                                // Convert to text completion chunk format
                                let sse_chunk = StreamChunkResponse {
                                    id: id.clone(),
                                    object: "text_completion".to_string(),
                                    created,
                                    model: model.clone(),
                                    choices: chunk.choices.into_iter().map(|choice| StreamChoice {
                                        index: choice.index,
                                        delta: Delta {
                                            role: None,
                                            content: choice.delta.content,
                                        },
                                        finish_reason: choice.finish_reason,
                                    }).collect(),
                                    usage: chunk.usage.map(|u| Usage {
                                        prompt_tokens: u.prompt_tokens,
                                        completion_tokens: u.completion_tokens,
                                        total_tokens: u.total_tokens,
                                    }),
                                };
                                
                                let json = serde_json::to_string(&sse_chunk).unwrap_or_default();
                                Some(Ok::<_, Infallible>(Event::default().data(json)))
                            }
                            Err(e) => {
                                tracing::error!("Stream error: {:?}", e);
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
            }
            Err(domain_error) => {
                let status_code = map_domain_error_to_status(&domain_error);
                (status_code, ResponseJson::<ErrorResponse>(domain_error.into())).into_response()
            }
        }
    } else {
        // Handle non-streaming response
        match domain.text_completion(domain_params).await {
            Ok(result) => {
                let response = completion_to_http_response(
                    result,
                    &request.model,
                    format!("cmpl-{}", generate_completion_id()),
                    current_unix_timestamp(),
                );
                (StatusCode::OK, ResponseJson(response)).into_response()
            }
            Err(domain_error) => {
                let status_code = map_domain_error_to_status(&domain_error);
                (status_code, ResponseJson::<ErrorResponse>(domain_error.into())).into_response()
            }
        }
    }
}

pub async fn models(
    State(domain): State<AppState>,
) -> Result<ResponseJson<ModelsResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    match domain.get_available_models().await {
        Ok(models) => {
            let response = ModelsResponse {
                object: "list".to_string(),
                data: models.into_iter().map(|model| ModelInfo {
                    id: model.id,
                    object: "model".to_string(),
                    created: model.created.unwrap_or(current_unix_timestamp()),
                    owned_by: model.owned_by.unwrap_or_else(|| model.provider.clone()),
                }).collect(),
            };
            Ok(ResponseJson(response))
        }
        Err(domain_error) => {
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(domain_error.into())
            ))
        }
    }
}

pub async fn quote(
    State(domain): State<AppState>,
) -> Result<ResponseJson<QuoteResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    match domain.get_quote().await {
        Ok(quote_response) => {
            let response: QuoteResponse = quote_response.into();
            Ok(ResponseJson(response))
        }
        Err(domain_error) => {
            let status_code = map_domain_error_to_status(&domain_error);
            Err((status_code, ResponseJson(domain_error.into())))
        }
    }
}
