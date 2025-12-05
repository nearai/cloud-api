use crate::{
    middleware::{auth::AuthenticatedApiKey, RequestBodyHash},
    models::{ErrorResponse, ResponseInputItemList},
};
use axum::{
    body::Body,
    extract::{Extension, Json, Path, Query, State},
    http::{header, Response, StatusCode},
    response::{IntoResponse, Json as ResponseJson},
};
use bytes::Bytes;
use futures::stream::StreamExt;
use serde::Deserialize;
use services::attestation::ports::AttestationServiceTrait;
use services::responses::errors::ResponseError as ServiceResponseError;
use services::responses::models::*;
use services::responses::ports::ResponseServiceTrait;
use services::responses::service::ResponseServiceImpl;
use sha2::{Digest, Sha256};
use std::convert::Infallible;
use std::sync::Arc;
use tracing::debug;
use uuid::Uuid;

// Helper function to convert service ResponseContentItem to API ResponseContentPart (input-only)
fn convert_to_input_part(
    item: services::responses::models::ResponseContentItem,
) -> Option<crate::models::ResponseContentPart> {
    match item {
        ResponseContentItem::InputText { text } => {
            Some(crate::models::ResponseContentPart::InputText { text })
        }
        ResponseContentItem::InputImage { image_url, detail } => {
            Some(crate::models::ResponseContentPart::InputImage { image_url, detail })
        }
        ResponseContentItem::InputFile { file_id, detail } => {
            Some(crate::models::ResponseContentPart::InputFile { file_id, detail })
        }
        ResponseContentItem::OutputText { text, .. } => {
            // Backward compatibility: check for legacy file reference
            match crate::routes::common::parse_legacy_file_reference(&text) {
                Ok(Some(file_id)) => Some(crate::models::ResponseContentPart::InputFile {
                    file_id,
                    detail: None,
                }),
                Ok(None) | Err(_) => Some(crate::models::ResponseContentPart::InputText { text }),
            }
        }
        ResponseContentItem::ToolCalls { .. } => None,
    }
}

// Helper functions for error mapping
fn map_response_error_to_status(error: &ServiceResponseError) -> StatusCode {
    match error {
        ServiceResponseError::InvalidParams(_) => StatusCode::BAD_REQUEST,
        ServiceResponseError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ServiceResponseError::UnknownTool(_) => StatusCode::BAD_REQUEST,
        ServiceResponseError::EmptyToolName => StatusCode::BAD_REQUEST,
    }
}

/// Compute SHA256 hash of data
fn compute_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

impl From<ServiceResponseError> for ErrorResponse {
    fn from(error: ServiceResponseError) -> Self {
        match error {
            ServiceResponseError::InvalidParams(msg) => {
                ErrorResponse::new(msg, "invalid_request_error".to_string())
            }
            ServiceResponseError::InternalError(msg) => ErrorResponse::new(
                format!("Internal server error: {msg}"),
                "internal_server_error".to_string(),
            ),
            ServiceResponseError::UnknownTool(msg) => ErrorResponse::new(
                format!("Unknown tool: {msg}"),
                "invalid_request_error".to_string(),
            ),
            ServiceResponseError::EmptyToolName => ErrorResponse::new(
                "Tool call is missing a tool name".to_string(),
                "invalid_request_error".to_string(),
            ),
        }
    }
}

// State for response routes
#[derive(Clone)]
pub struct ResponseRouteState {
    pub response_service: Arc<ResponseServiceImpl>,
    pub attestation_service: Arc<dyn AttestationServiceTrait>,
}

/// Create response
///
/// Generate an AI response for a conversation with tool calling and streaming support.
#[utoipa::path(
    post,
    path = "/v1/responses",
    tag = "Responses",
    request_body = CreateResponseRequest,
    responses(
        (status = 200, description = "Response created", body = ResponseObject),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 402, description = "Insufficient credits", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn create_response(
    State(state): State<ResponseRouteState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    Json(mut request): Json<CreateResponseRequest>,
) -> axum::response::Response {
    let service = state.response_service.clone();
    let attestation_service = state.attestation_service.clone();
    debug!(
        "Create response request from api key: {:?}",
        api_key.api_key.id
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

    // Set defaults for internal fields
    request.max_tool_calls = request.max_tool_calls.or(Some(10));
    request.store = request.store.or(Some(true));
    request.background = request.background.or(Some(false));
    request.text = request.text.or(Some(ResponseTextConfig {
        format: ResponseTextFormat::Text,
        verbosity: Some("medium".to_string()),
    }));
    request.reasoning = request
        .reasoning
        .or(Some(ResponseReasoningConfig { effort: None }));

    // Store model for logging before moving request
    let model = request.model.clone();

    // Check if streaming is requested
    if request.stream.unwrap_or(false) {
        tracing::debug!(
            user_id = %api_key.api_key.created_by_user_id.0,
            model = %model,
            "Processing streaming response request"
        );

        // Create streaming response
        match service
            .create_response_stream(
                request,
                services::UserId(api_key.api_key.created_by_user_id.0),
                api_key.api_key.id.0.clone(),
                api_key.organization.id.0,
                api_key.workspace.id.0,
                body_hash.hash.clone(),
            )
            .await
        {
            Ok(stream) => {
                tracing::debug!(
                    user_id = %api_key.api_key.created_by_user_id.0,
                    "Successfully created streaming response, returning SSE stream with signature accumulation"
                );

                // Shared state for accumulating bytes and tracking response_id
                let accumulated_bytes = Arc::new(tokio::sync::Mutex::new(Vec::new()));
                let response_id_state = Arc::new(tokio::sync::Mutex::new(None::<String>));
                let request_hash = body_hash.hash.clone();

                // Clone for closures
                let accumulated_clone = accumulated_bytes.clone();
                let response_id_clone = response_id_state.clone();
                let attestation_clone = attestation_service.clone();

                // Format events as SSE bytes and accumulate them
                let byte_stream = stream.then(move |event| {
                    let accumulated_inner = accumulated_clone.clone();
                    let response_id_inner = response_id_clone.clone();
                    let attestation_inner = attestation_clone.clone();
                    let request_hash_inner = request_hash.clone();
                    async move {
                        // Extract response_id from response.created event
                        if event.event_type == "response.created" {
                            if let Some(ref response) = event.response {
                                let mut rid = response_id_inner.lock().await;
                                if rid.is_none() {
                                    *rid = Some(response.id.clone());
                                    tracing::debug!("Extracted response_id: {}", response.id);
                                }
                            }
                        }

                        // Format as SSE: "event: {type}\ndata: {json}\n\n"
                        let json = serde_json::to_string(&event).unwrap_or_default();
                        let sse_bytes = format!("event: {}\ndata: {}\n\n", event.event_type, json);
                        let bytes = Bytes::from(sse_bytes);

                        // Accumulate bytes synchronously - this ensures all bytes are captured
                        // before the stream chunk is yielded to the client
                        accumulated_inner.lock().await.extend_from_slice(&bytes);

                        // Check if stream is completing - store signature
                        if event.event_type == "response.completed" {
                            // At this point, all bytes have been accumulated synchronously
                            // Now we can safely compute the hash and store the signature
                            let bytes_accumulated = accumulated_inner.lock().await.clone();
                            let response_hash = compute_sha256(&bytes_accumulated);
                            if let Some(rid) = response_id_inner.lock().await.as_ref() {
                                let rid = rid.clone();
                                let req_hash = request_hash_inner.clone();
                                let attest = attestation_inner.clone();
                                tracing::debug!(
                                    "Storing signature for response_id: {}, request_hash: {}, response_hash: {}",
                                    rid, req_hash, response_hash
                                );

                                // Spawn task to store signature asynchronously (doesn't block stream)
                                // but we've already computed the hash with complete data
                                tokio::spawn(async move {
                                    // Store both ECDSA and ED25519 signatures
                                    if let Err(e) = attest.store_response_signature(
                                        &rid,
                                        req_hash.clone(),
                                        response_hash.clone(),
                                    ).await {
                                        tracing::error!("Failed to store response signature: {}", e);
                                    } else {
                                        tracing::debug!("Successfully stored signature for response_id: {}", rid);
                                    }
                                });
                            }
                        }

                        Ok::<Bytes, Infallible>(bytes)
                    }
                });

                // Return as raw byte stream with SSE headers
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header(header::CONNECTION, "keep-alive")
                    .body(Body::from_stream(byte_stream))
                    .unwrap()
            }
            Err(error) => {
                tracing::error!(
                    user_id = %api_key.api_key.created_by_user_id.0,
                    model = %model,
                    error = %error,
                    "Failed to create streaming response"
                );
                let status_code = map_response_error_to_status(&error);
                (status_code, ResponseJson::<ErrorResponse>(error.into())).into_response()
            }
        }
    } else {
        tracing::debug!(
            user_id = %api_key.api_key.created_by_user_id.0,
            model = %model,
            "Processing non-streaming response request"
        );

        // Service only supports streaming - collect stream for non-streaming response
        match service
            .create_response_stream(
                request.clone(),
                services::UserId(api_key.api_key.created_by_user_id.0),
                api_key.api_key.id.0.clone(),
                api_key.organization.id.0,
                api_key.workspace.id.0,
                body_hash.hash.clone(),
            )
            .await
        {
            Ok(stream) => {
                tracing::debug!(
                    user_id = %api_key.api_key.created_by_user_id.0,
                    "Successfully created stream, collecting events for non-streaming response"
                );

                // Collect stream events to build complete response
                let mut response_id = None;
                let mut content = String::new();
                let mut status = ResponseStatus::InProgress;
                let mut final_response: Option<ResponseObject> = None;

                let mut stream = Box::pin(stream);
                let mut event_count = 0;
                let mut delta_count = 0;
                while let Some(event) = stream.next().await {
                    event_count += 1;
                    tracing::debug!(
                        "Non-streaming collection: received event #{} type={} delta={:?}",
                        event_count,
                        event.event_type,
                        event.delta
                    );
                    match event.event_type.as_str() {
                        "response.created" => {
                            // Extract response ID from response object
                            if let Some(response) = &event.response {
                                response_id = Some(response.id.clone());
                                tracing::debug!(
                                    "Non-streaming: extracted response_id={}",
                                    response.id
                                );
                            }
                        }
                        "response.output_text.delta" => {
                            // Accumulate content deltas
                            if let Some(delta) = &event.delta {
                                delta_count += 1;
                                tracing::debug!(
                                    "Non-streaming: delta #{} len={} content='{}'",
                                    delta_count,
                                    delta.len(),
                                    delta
                                );
                                content.push_str(delta);
                            }
                        }
                        "response.completed" => {
                            status = ResponseStatus::Completed;
                            tracing::debug!(
                                "Non-streaming: response.completed event, accumulated_content_len={}",
                                content.len()
                            );
                            // The response object is already in the right format
                            if let Some(response_obj) = event.response {
                                tracing::debug!(
                                    "Non-streaming: response.completed has response object"
                                );
                                {
                                    tracing::debug!(
                                        "Non-streaming: parsed ResponseObject, checking output text"
                                    );
                                    // Log the output text from the final response
                                    for (idx, output_item) in response_obj.output.iter().enumerate()
                                    {
                                        if let ResponseOutputItem::Message {
                                            content: msg_content,
                                            ..
                                        } = output_item
                                        {
                                            for (cidx, content_part) in
                                                msg_content.iter().enumerate()
                                            {
                                                if let ResponseContentItem::OutputText {
                                                    text,
                                                    ..
                                                } = content_part
                                                {
                                                    tracing::debug!(
                                                        "Non-streaming: final_response output[{}].content[{}] text_len={} text='{}'",
                                                        idx, cidx, text.len(), text
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    final_response = Some(response_obj);
                                }
                            }
                        }
                        "response.failed" => {
                            status = ResponseStatus::Failed;
                        }
                        _ => {
                            // Handle other events as needed
                        }
                    }
                }
                tracing::info!(
                    "Non-streaming: collected {} events, {} deltas, accumulated_content_len={}",
                    event_count,
                    delta_count,
                    content.len()
                );

                // Use final response from completed event or build fallback response
                let response = if let Some(final_resp) = final_response {
                    // Use the complete response object from the response.completed event
                    final_resp
                } else {
                    // Fallback: Build response from collected data (for compatibility)
                    // Trim accumulated content to remove leading/trailing whitespace
                    let trimmed_content = content.trim().to_string();
                    let resp_id =
                        response_id.unwrap_or_else(|| format!("resp_{}", Uuid::new_v4().simple()));
                    ResponseObject {
                        id: resp_id.clone(),
                        object: "response".to_string(),
                        created_at: chrono::Utc::now().timestamp(),
                        status,
                        background: request.background.unwrap_or(false),
                        conversation: request.conversation.as_ref().map(|conv_ref| {
                            let id = match conv_ref {
                                services::responses::models::ConversationReference::Id(id) => {
                                    id.clone()
                                }
                                services::responses::models::ConversationReference::Object {
                                    id,
                                    ..
                                } => id.clone(),
                            };
                            services::responses::models::ConversationResponseReference { id }
                        }),
                        error: None,
                        incomplete_details: None,
                        instructions: request.instructions,
                        max_output_tokens: request.max_output_tokens,
                        max_tool_calls: request.max_tool_calls,
                        model: request.model.clone(),
                        output: vec![ResponseOutputItem::Message {
                            id: format!("msg_{}", Uuid::new_v4().simple()),
                            response_id: resp_id.clone(),
                            previous_response_id: request.previous_response_id.clone(),
                            next_response_ids: vec![],
                            created_at: chrono::Utc::now().timestamp(),
                            status: ResponseItemStatus::Completed,
                            role: "assistant".to_string(),
                            content: vec![ResponseContentItem::OutputText {
                                text: trimmed_content,
                                annotations: vec![],
                                logprobs: vec![],
                            }],
                            model: request.model,
                        }],
                        parallel_tool_calls: request.parallel_tool_calls.unwrap_or(false),
                        previous_response_id: request.previous_response_id.clone(),
                        next_response_ids: vec![],
                        prompt_cache_key: request.prompt_cache_key,
                        prompt_cache_retention: None,
                        reasoning: None,
                        safety_identifier: request.safety_identifier,
                        service_tier: "default".to_string(),
                        store: request.store.unwrap_or(false),
                        temperature: request.temperature.unwrap_or(1.0),
                        text: request.text,
                        tool_choice: ResponseToolChoiceOutput::Auto("auto".to_string()),
                        tools: request.tools.unwrap_or_default(),
                        top_logprobs: 0,
                        top_p: request.top_p.unwrap_or(1.0),
                        truncation: "disabled".to_string(),
                        usage: Usage::new(0, 0), // TODO: Get actual usage from stream
                        user: None,
                        metadata: request.metadata,
                    }
                };

                debug!(
                    "Created response {} for key {}",
                    response.id, api_key.api_key.created_by_user_id.0
                );
                (StatusCode::OK, ResponseJson(response)).into_response()
            }
            Err(error) => {
                tracing::error!(
                    user_id = %api_key.api_key.created_by_user_id.0,
                    model = %model,
                    error = %error,
                    "Failed to create non-streaming response"
                );
                let status_code = map_response_error_to_status(&error);
                (status_code, ResponseJson::<ErrorResponse>(error.into())).into_response()
            }
        }
    }
}

/// Get a response by ID
///
/// Retrieve details of a specific response.
#[utoipa::path(
    get,
    path = "/v1/responses/{response_id}",
    tag = "Responses",
    params(
        ("response_id" = String, Path, description = "Response ID")
    ),
    responses(
        (status = 200, description = "Response details", body = ResponseObject),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 404, description = "Response not found", body = ErrorResponse),
        (status = 501, description = "Not implemented", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn get_response(
    Path(_response_id): Path<String>,
    Query(_params): Query<GetResponseQuery>,
    State(_state): State<ResponseRouteState>,
    Extension(_api_key): Extension<services::workspace::ApiKey>,
) -> Result<ResponseJson<ResponseObject>, (StatusCode, ResponseJson<ErrorResponse>)> {
    // TODO: Implement get_response method in ResponseService
    Err((
        StatusCode::NOT_IMPLEMENTED,
        ResponseJson(ErrorResponse::new(
            "Get response not yet implemented".to_string(),
            "not_implemented".to_string(),
        )),
    ))
}

/// Delete a response
///
/// Delete a specific response.
#[utoipa::path(
    delete,
    path = "/v1/responses/{response_id}",
    tag = "Responses",
    params(
        ("response_id" = String, Path, description = "Response ID")
    ),
    responses(
        (status = 200, description = "Response deleted successfully"),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 404, description = "Response not found", body = ErrorResponse),
        (status = 501, description = "Not implemented", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn delete_response(
    Path(_response_id): Path<String>,
    State(_state): State<ResponseRouteState>,
    Extension(_api_key): Extension<services::workspace::ApiKey>,
) -> Result<ResponseJson<ResponseDeleteResult>, (StatusCode, ResponseJson<ErrorResponse>)> {
    // TODO: Implement delete_response method in ResponseService
    Err((
        StatusCode::NOT_IMPLEMENTED,
        ResponseJson(ErrorResponse::new(
            "Delete response not yet implemented".to_string(),
            "not_implemented".to_string(),
        )),
    ))
}

/// Cancel a response (for background responses)
///
/// Cancel an in-progress background response.
#[utoipa::path(
    post,
    path = "/v1/responses/{response_id}/cancel",
    tag = "Responses",
    params(
        ("response_id" = String, Path, description = "Response ID")
    ),
    responses(
        (status = 200, description = "Response cancelled successfully", body = ResponseObject),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 404, description = "Response not found", body = ErrorResponse),
        (status = 501, description = "Not implemented", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn cancel_response(
    Path(_response_id): Path<String>,
    State(_state): State<ResponseRouteState>,
    Extension(_api_key): Extension<services::workspace::ApiKey>,
) -> Result<ResponseJson<ResponseObject>, (StatusCode, ResponseJson<ErrorResponse>)> {
    // TODO: Implement cancel_response method in ResponseService
    Err((
        StatusCode::NOT_IMPLEMENTED,
        ResponseJson(ErrorResponse::new(
            "Cancel response not yet implemented".to_string(),
            "not_implemented".to_string(),
        )),
    ))
}

/// List input items for a response
///
/// Retrieve all input items (user messages and files) for a specific response.
#[utoipa::path(
    get,
    path = "/v1/responses/{response_id}/input_items",
    tag = "Responses",
    params(
        ("response_id" = String, Path, description = "Response ID")
    ),
    responses(
        (status = 200, description = "List of input items", body = ResponseInputItemList),
        (status = 400, description = "Invalid response ID", body = ErrorResponse),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 404, description = "Response not found", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn list_input_items(
    Path(response_id): Path<String>,
    Query(params): Query<ListInputItemsQuery>,
    State(state): State<ResponseRouteState>,
    Extension(auth): Extension<AuthenticatedApiKey>,
) -> Result<ResponseJson<ResponseInputItemList>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let service = state.response_service.clone();
    debug!(
        "List input items for response {} from workspace {}",
        response_id, auth.workspace.id.0
    );

    // Parse response ID (format: "resp_{uuid}")
    let response_uuid = response_id
        .strip_prefix("resp_")
        .unwrap_or(&response_id)
        .parse::<Uuid>()
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    "Invalid response ID format".to_string(),
                    "invalid_request_error".to_string(),
                )),
            )
        })?;

    let parsed_response_id = ResponseId(response_uuid);

    // Verify the response belongs to this workspace
    match service
        .response_repository
        .get_by_id(parsed_response_id.clone(), auth.workspace.id.clone())
        .await
    {
        Ok(Some(_)) => {
            // Response exists and belongs to workspace, proceed
        }
        Ok(None) => {
            return Err((
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    "Response not found".to_string(),
                    "not_found".to_string(),
                )),
            ));
        }
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    format!("Failed to fetch response: {e}"),
                    "internal_server_error".to_string(),
                )),
            ));
        }
    }

    // Get all response items
    let items = service
        .response_items_repository
        .list_by_response(parsed_response_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    format!("Failed to fetch response items: {e}"),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    // Filter to only user input items and convert to API format
    let mut input_items: Vec<crate::models::ResponseInputItem> = Vec::new();

    for item in items {
        if let ResponseOutputItem::Message { role, content, .. } = item {
            if role == "user" {
                // Convert service ResponseContentItem to API ResponseContentPart (input-only)
                // This provides type safety - only input variants can exist here
                let api_content: Vec<crate::models::ResponseContentPart> = content
                    .into_iter()
                    .filter_map(convert_to_input_part)
                    .collect();

                input_items.push(crate::models::ResponseInputItem {
                    role,
                    content: crate::models::ResponseContent::Parts(api_content),
                });
            }
        }
    }

    // Apply pagination if needed (for now, return all)
    let limit = params.limit.unwrap_or(100).min(1000);
    let has_more = input_items.len() > limit as usize;
    let input_items: Vec<_> = input_items.into_iter().take(limit as usize).collect();

    let first_id = if input_items.is_empty() {
        String::new()
    } else {
        "0".to_string()
    };
    let last_id = if input_items.is_empty() {
        String::new()
    } else {
        (input_items.len() - 1).to_string()
    };

    Ok(ResponseJson(ResponseInputItemList {
        object: "list".to_string(),
        data: input_items,
        first_id,
        last_id,
        has_more,
    }))
}

// Query parameter structs
#[derive(Debug, Deserialize)]
pub struct GetResponseQuery {
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub include_obfuscation: Option<bool>,
    pub starting_after: Option<i64>,
    pub stream: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ListInputItemsQuery {
    pub after: Option<String>,
    pub include: Option<Vec<String>>,
    pub limit: Option<i64>,
    pub order: Option<String>, // "asc" or "desc"
}
