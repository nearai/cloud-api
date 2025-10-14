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
    body_hash: RequestBodyHash,
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
        body_hash: body_hash.hash.clone(),
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
        api_key_id,
        organization_id,
        workspace_id,
        metadata: None,
        body_hash: body_hash.hash.clone(),
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

    // Convert HTTP request to service parameters
    let service_request = convert_chat_request_to_service(
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
        .create_completion_stream(service_request)
        .await
    {
        Ok(stream) => {
            // Check if streaming is requested
            if request.stream == Some(true) {
                // Convert to raw bytes stream with proper SSE formatting
                let byte_stream = stream
                    .map(|result| match result {
                        Ok(event) => {
                            // raw_bytes contains "data: {...}\n", extract just the JSON part
                            let raw_str = String::from_utf8_lossy(&event.raw_bytes);
                            let json_data = raw_str
                                .trim()
                                .strip_prefix("data: ")
                                .unwrap_or(raw_str.trim())
                                .to_string();
                            tracing::info!("Completion stream event: {}", json_data);
                            // Format as SSE event with proper newlines
                            Ok::<Bytes, Infallible>(Bytes::from(format!("data: {}\n\n", json_data)))
                        }
                        Err(e) => {
                            tracing::error!("Completion stream error: {}", e);
                            Ok::<Bytes, Infallible>(Bytes::from(format!("data: error: {}\n\n", e)))
                        }
                    })
                    .chain(futures::stream::once(async move {
                        // Send [DONE] with 3 newlines total (1 after [DONE], then 2 more for proper SSE termination)
                        Ok::<Bytes, Infallible>(Bytes::from_static(b"data: [DONE]\n\n\n"))
                    }));

                // Return raw streaming response with SSE headers
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header(header::CONNECTION, "keep-alive")
                    .body(Body::from_stream(byte_stream))
                    .unwrap()
            } else {
                unimplemented!()
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
    Extension(body_hash): Extension<RequestBodyHash>,
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
        body_hash,
    );

    // Call the completion service - it handles usage tracking internally
    match app_state
        .completion_service
        .create_completion_stream(service_request)
        .await
    {
        Ok(_stream) => {
            unimplemented!()
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
