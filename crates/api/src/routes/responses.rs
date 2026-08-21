use crate::{
    middleware::{auth::AuthenticatedApiKey, RequestBodyHash, RequestCorrelation},
    models::ErrorResponse,
    routes::extractors::OpenAiJson,
};
use axum::{
    body::Body,
    extract::{Extension, State},
    http::{header, HeaderMap, Response, StatusCode},
    response::{IntoResponse, Json as ResponseJson},
};
use bytes::Bytes;
use futures::stream::StreamExt;
use services::attestation::ports::AttestationServiceTrait;
use services::responses::errors::ResponseError as ServiceResponseError;
use services::responses::models::*;
use services::responses::ports::ResponseServiceTrait;
use services::responses::service::ResponseServiceImpl;
use sha2::{Digest, Sha256};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::debug;

/// Bound best-effort attestation persistence so a database problem never
/// prevents a completed inference response from being delivered.
const RESPONSE_ATTESTATION_STORE_TIMEOUT: Duration = Duration::from_secs(5);

/// OpenAPI-only view of the stateless Responses request contract.
///
/// The runtime request type keeps legacy variants so it can return a precise
/// `invalid_request_error` for them. The public endpoint schema should instead
/// show only the items accepted by the stateless implementation.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct StatelessCreateResponseRequestSchema {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<StatelessResponseInputSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Must be `false` when supplied; omitted is normalized to `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(default = false, example = false)]
    pub store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<StatelessResponseToolSchema>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ResponseToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ResponseReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety_identifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub service_tier: Option<inference_providers::ChatServiceTier>,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum StatelessResponseInputSchema {
    Text(String),
    Items(Vec<StatelessResponseInputItemSchema>),
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum StatelessResponseInputItemSchema {
    FunctionCall {
        #[serde(rename = "type")]
        type_: FunctionCallType,
        call_id: String,
        name: String,
        arguments: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
    },
    FunctionCallOutput {
        #[serde(rename = "type")]
        type_: FunctionCallOutputType,
        call_id: String,
        output: String,
    },
    Message {
        role: String,
        content: StatelessResponseContentSchema,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum StatelessResponseContentSchema {
    Text(String),
    Parts(Vec<StatelessResponseContentPartSchema>),
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(tag = "type")]
pub enum StatelessResponseContentPartSchema {
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(rename = "output_text")]
    OutputText { text: String },
    #[serde(rename = "input_image")]
    InputImage {
        image_url: ResponseImageUrl,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(tag = "type")]
pub enum StatelessResponseToolSchema {
    #[serde(rename = "function")]
    Function {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        parameters: Option<serde_json::Value>,
    },
}

// Helper functions for error mapping
fn map_response_error_to_status(error: &ServiceResponseError) -> StatusCode {
    match error {
        ServiceResponseError::InvalidParams(_) => StatusCode::BAD_REQUEST,
        ServiceResponseError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ServiceResponseError::Completion(error) => {
            crate::routes::common::map_domain_error_to_status(error)
        }
        ServiceResponseError::UnknownTool(_) => StatusCode::BAD_REQUEST,
        ServiceResponseError::EmptyToolName => StatusCode::BAD_REQUEST,
        ServiceResponseError::StreamInterrupted => StatusCode::INTERNAL_SERVER_ERROR,
        ServiceResponseError::ConversationNotFound => StatusCode::NOT_FOUND,
        ServiceResponseError::PreviousResponseNotFound => StatusCode::NOT_FOUND,
        ServiceResponseError::McpConnectionFailed(_) => StatusCode::BAD_GATEWAY,
        ServiceResponseError::McpToolDiscoveryFailed(_) => StatusCode::BAD_GATEWAY,
        ServiceResponseError::McpToolExecutionFailed(_) => StatusCode::BAD_GATEWAY,
        ServiceResponseError::McpServerLimitExceeded { .. } => StatusCode::BAD_REQUEST,
        ServiceResponseError::McpToolLimitExceeded { .. } => StatusCode::BAD_REQUEST,
        ServiceResponseError::McpInsecureUrl => StatusCode::BAD_REQUEST,
        ServiceResponseError::McpPrivateIpBlocked => StatusCode::BAD_REQUEST,
        ServiceResponseError::McpApprovalRequired { .. } => StatusCode::BAD_REQUEST,
        ServiceResponseError::McpApprovalRequestNotFound(_) => StatusCode::NOT_FOUND,
        ServiceResponseError::FunctionCallRequired { .. } => StatusCode::BAD_REQUEST,
        ServiceResponseError::FunctionCallNotFound(_) => StatusCode::NOT_FOUND,
    }
}

fn status_code_from_response_event(status_code: Option<u16>) -> StatusCode {
    status_code
        .and_then(|code| StatusCode::from_u16(code).ok())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

fn error_response_from_response_event(
    error: services::responses::models::ResponseError,
) -> ErrorResponse {
    let mut response = ErrorResponse::new(error.message, error.type_);
    response.error.param = error.param;
    response.error.code = error.code;
    response
}

impl From<ServiceResponseError> for ErrorResponse {
    fn from(error: ServiceResponseError) -> Self {
        match error {
            ServiceResponseError::InvalidParams(msg) => {
                ErrorResponse::new(msg, "invalid_request_error".to_string())
            }
            ServiceResponseError::Completion(error) => error.into(),
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
            ServiceResponseError::StreamInterrupted => {
                ErrorResponse::new("Stream interrupted".to_string(), "stream_error".to_string())
            }
            ServiceResponseError::ConversationNotFound => ErrorResponse::new(
                "Conversation not found".to_string(),
                "not_found_error".to_string(),
            ),
            ServiceResponseError::PreviousResponseNotFound => ErrorResponse::new(
                "Previous response not found".to_string(),
                "not_found_error".to_string(),
            ),
            ServiceResponseError::McpConnectionFailed(msg) => ErrorResponse::new(
                format!("MCP connection failed: {msg}"),
                "mcp_error".to_string(),
            ),
            ServiceResponseError::McpToolDiscoveryFailed(msg) => ErrorResponse::new(
                format!("MCP tool discovery failed: {msg}"),
                "mcp_error".to_string(),
            ),
            ServiceResponseError::McpToolExecutionFailed(msg) => ErrorResponse::new(
                format!("MCP tool execution failed: {msg}"),
                "mcp_error".to_string(),
            ),
            ServiceResponseError::McpServerLimitExceeded { max } => ErrorResponse::new(
                format!("MCP server limit exceeded: max {max} servers per request"),
                "invalid_request_error".to_string(),
            ),
            ServiceResponseError::McpToolLimitExceeded { server, count, max } => {
                ErrorResponse::new(
                    format!(
                        "MCP tool limit exceeded: server '{server}' has {count} tools, max {max}"
                    ),
                    "invalid_request_error".to_string(),
                )
            }
            ServiceResponseError::McpInsecureUrl => ErrorResponse::new(
                "MCP server URL must use HTTPS".to_string(),
                "invalid_request_error".to_string(),
            ),
            ServiceResponseError::McpPrivateIpBlocked => ErrorResponse::new(
                "MCP private IP addresses not allowed".to_string(),
                "invalid_request_error".to_string(),
            ),
            ServiceResponseError::McpApprovalRequired { server, tool } => ErrorResponse::new(
                format!("MCP approval required for tool '{tool}' on server '{server}'"),
                "mcp_approval_required".to_string(),
            ),
            ServiceResponseError::McpApprovalRequestNotFound(msg) => ErrorResponse::new(
                format!("MCP approval request not found: {msg}"),
                "not_found_error".to_string(),
            ),
            ServiceResponseError::FunctionCallRequired { name, call_id } => ErrorResponse::new(
                format!("Function call required: {name} (call_id: {call_id})"),
                "function_call_required".to_string(),
            ),
            ServiceResponseError::FunctionCallNotFound(msg) => ErrorResponse::new(
                format!("Function call not found: {msg}"),
                "not_found_error".to_string(),
            ),
        }
    }
}

// State for response routes
#[derive(Clone)]
pub struct ResponseRouteState {
    pub response_service: Arc<ResponseServiceImpl>,
    /// Existing completed-response gateway attestation remains best-effort.
    /// Its stored material is a response ID plus signatures over
    /// request/response digests, never raw response or item records.
    pub attestation_service: Arc<dyn AttestationServiceTrait>,
}

/// Request-local state used to sign the exact SSE bytes returned to a client.
///
/// The state deliberately retains a running digest rather than accumulating
/// stream content, so no response payload is kept after an event is emitted.
struct StreamingResponseAttestation {
    response_id: Option<String>,
    response_hasher: Sha256,
    completed: bool,
}

impl Default for StreamingResponseAttestation {
    fn default() -> Self {
        Self {
            response_id: None,
            response_hasher: Sha256::new(),
            completed: false,
        }
    }
}

impl StreamingResponseAttestation {
    /// Record one client-visible SSE frame and return attestation material when
    /// the response has completed. The returned digest includes the completed
    /// frame itself.
    fn record_event(
        &mut self,
        event: &ResponseStreamEvent,
        sse_bytes: &[u8],
    ) -> Option<(String, String)> {
        if self.response_id.is_none() {
            self.response_id = event.response.as_ref().map(|response| response.id.clone());
        }

        self.response_hasher.update(sse_bytes);

        if self.completed || event.event_type != "response.completed" {
            return None;
        }
        self.completed = true;

        self.response_id.clone().map(|response_id| {
            let response_hash = hex::encode(self.response_hasher.clone().finalize());
            (response_id, response_hash)
        })
    }
}

/// Persist the minimal metadata needed to retrieve an attestation later.
///
/// Attestation failures must not change the inference result. In particular,
/// do not include provider/database error strings here: they may contain
/// request-derived data or infrastructure details.
async fn persist_response_attestation(
    attestation_service: &dyn AttestationServiceTrait,
    response_id: &str,
    request_hash: String,
    response_hash: String,
) {
    match tokio::time::timeout(
        RESPONSE_ATTESTATION_STORE_TIMEOUT,
        attestation_service.store_response_signature(response_id, request_hash, response_hash),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            tracing::warn!(%response_id, "Response attestation persistence failed");
        }
        Err(_) => {
            tracing::warn!(%response_id, "Response attestation persistence timed out");
        }
    }
}

/// Store an attestation for a non-streaming response before sending it.
async fn persist_non_streaming_response_attestation(
    attestation_service: &dyn AttestationServiceTrait,
    response: &ResponseObject,
    request_hash: String,
) {
    // This serialization is request-local and is immediately reduced to a
    // digest. The response itself is not written to the attestation store.
    let response_json = serde_json::to_vec(response).expect("response serialization failed");
    let response_hash = hex::encode(Sha256::digest(&response_json));
    persist_response_attestation(
        attestation_service,
        &response.id,
        request_hash,
        response_hash,
    )
    .await;
}

/// Convert response events into signed SSE frames. The response-completed
/// frame is held until its best-effort attestation write has been attempted.
/// A successful write is available for subsequent `resp_*` lookup; a failed or
/// timed-out write does not change the inference result.
fn signed_response_sse_stream(
    stream: Pin<Box<dyn futures::Stream<Item = ResponseStreamEvent> + Send>>,
    attestation_service: Arc<dyn AttestationServiceTrait>,
    request_hash: String,
) -> Pin<Box<dyn futures::Stream<Item = Result<Bytes, Infallible>> + Send>> {
    let signature_state = Arc::new(Mutex::new(StreamingResponseAttestation::default()));

    Box::pin(stream.then(move |event| {
        let signature_state = signature_state.clone();
        let attestation_service = attestation_service.clone();
        let request_hash = request_hash.clone();

        async move {
            let json = serde_json::to_string(&event).expect("event serialization failed");
            let sse_bytes = format!("event: {}\ndata: {}\n\n", event.event_type, json);

            let signature = {
                let mut state = signature_state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.record_event(&event, sse_bytes.as_bytes())
            };

            if let Some((response_id, response_hash)) = signature {
                persist_response_attestation(
                    attestation_service.as_ref(),
                    &response_id,
                    request_hash,
                    response_hash,
                )
                .await;
            }

            Ok::<Bytes, Infallible>(Bytes::from(sse_bytes))
        }
    }))
}

/// Return an explicit migration response for the retired response-history API.
///
/// The route is mounted behind the standard API-key middleware.  Keeping it
/// separate from the create endpoint makes it clear that only a new, single
/// stateless request is supported.
pub async fn response_history_gone() -> axum::response::Response {
    (
        StatusCode::GONE,
        ResponseJson(ErrorResponse::new(
            "Response history is unavailable because the Responses API is stateless.".to_string(),
            "gone".to_string(),
        )),
    )
        .into_response()
}

/// Create response
///
/// Generate a single-turn, stateless AI response with optional streaming.
#[utoipa::path(
    post,
    path = "/v1/responses",
    tag = "Responses",
    request_body = StatelessCreateResponseRequestSchema,
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
    Extension(correlation): Extension<RequestCorrelation>,
    headers: HeaderMap,
    OpenAiJson(mut request): OpenAiJson<CreateResponseRequest>,
) -> axum::response::Response {
    let service = state.response_service.clone();
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

    if let Err(error) = request.validate_stateless() {
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

    let signing_algo = encryption_headers.signing_algo;
    let client_pub_key = encryption_headers.client_pub_key;
    let model_pub_key = encryption_headers.model_pub_key;
    let encryption_version = encryption_headers.encryption_version;

    // Encryption requires streaming mode because encrypted chunks from vLLM are independently
    // encrypted and cannot be concatenated. Non-streaming mode would produce corrupted data.
    if signing_algo.is_some() && client_pub_key.is_some() && request.stream != Some(true) {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Non-streaming mode is not supported with encryption. Use stream=true.".to_string(),
                "encryption_requires_streaming".to_string(),
            )),
        )
            .into_response();
    }

    // Set defaults for internal fields
    request.max_tool_calls = request.max_tool_calls.or(Some(10));
    request.store = Some(false);
    request.background = request.background.or(Some(false));
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
                correlation.request_id,
                api_key.organization.id.0,
                api_key.workspace.id.0,
                body_hash.hash.clone(),
                signing_algo.clone(),
                client_pub_key.clone(),
                model_pub_key.clone(),
                encryption_version.clone(),
            )
            .await
        {
            Ok(stream) => {
                tracing::debug!(
                    user_id = %api_key.api_key.created_by_user_id.0,
                    "Successfully created streaming response"
                );

                let byte_stream = signed_response_sse_stream(
                    stream,
                    state.attestation_service.clone(),
                    body_hash.hash.clone(),
                );

                // Return as raw byte stream with SSE headers
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-store")
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
                correlation.request_id,
                api_key.organization.id.0,
                api_key.workspace.id.0,
                body_hash.hash.clone(),
                signing_algo.clone(),
                client_pub_key.clone(),
                model_pub_key.clone(),
                encryption_version.clone(),
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
                let mut tracked_usage: Option<Usage> = None;
                let mut failed_error: Option<services::responses::models::ResponseError> = None;
                let mut failed_status_code: Option<u16> = None;

                let mut stream = Box::pin(stream);
                let mut event_count = 0;
                let mut delta_count = 0;
                while let Some(event) = stream.next().await {
                    event_count += 1;
                    let event_type = event.event_type.as_str();
                    let delta_len = event.delta.as_ref().map_or(0, String::len);
                    let has_delta = event.delta.is_some();
                    tracing::debug!(
                        event_count,
                        event_type,
                        has_delta,
                        delta_len,
                        "Non-streaming collection received event"
                    );
                    match event_type {
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
                                    "Non-streaming: delta #{} len={}",
                                    delta_count,
                                    delta.len()
                                );
                                content.push_str(delta);
                            }
                        }
                        "response.completed" => {
                            status = ResponseStatus::Completed;
                            if event.usage.is_some() {
                                tracked_usage = event.usage.clone();
                            }
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
                                                        "Non-streaming: final_response output[{}].content[{}] text_len={}",
                                                        idx, cidx, text.len()
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
                            failed_error = event.error.clone();
                            failed_status_code = event.status_code;
                            if event.usage.is_some() {
                                tracked_usage = event.usage.clone();
                            }
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

                if final_response.is_none() {
                    if let Some(error) = failed_error {
                        let status_code = status_code_from_response_event(failed_status_code);
                        let error_response = error_response_from_response_event(error);
                        return (status_code, ResponseJson(error_response)).into_response();
                    }
                }

                // Use final response from completed event or build fallback response
                let response = if let Some(final_resp) = final_response {
                    // Use the complete response object from the response.completed event
                    final_resp
                } else {
                    // Fallback: Build response from collected data (for compatibility)
                    // Trim accumulated content to remove leading/trailing whitespace
                    let trimmed_content = content.trim().to_string();
                    let resp_id = response_id
                        .unwrap_or_else(|| format!("resp_{}", uuid::Uuid::new_v4().simple()));
                    ResponseObject {
                        id: resp_id.clone(),
                        object: "response".to_string(),
                        created_at: chrono::Utc::now().timestamp(),
                        status,
                        background: false,
                        conversation: None,
                        error: None,
                        incomplete_details: None,
                        instructions: request.instructions,
                        max_output_tokens: request.max_output_tokens,
                        max_tool_calls: request.max_tool_calls,
                        model: request.model.clone(),
                        output: vec![ResponseOutputItem::Message {
                            id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
                            response_id: resp_id.clone(),
                            previous_response_id: None,
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
                            metadata: None,
                        }],
                        parallel_tool_calls: request.parallel_tool_calls.unwrap_or(false),
                        previous_response_id: None,
                        next_response_ids: vec![],
                        prompt_cache_key: request.prompt_cache_key,
                        prompt_cache_retention: None,
                        reasoning: None,
                        safety_identifier: request.safety_identifier,
                        service_tier: "default".to_string(),
                        store: false,
                        temperature: request.temperature.unwrap_or(1.0),
                        tool_choice: ResponseToolChoiceOutput::Auto("auto".to_string()),
                        tools: request.tools.unwrap_or_default(),
                        top_logprobs: 0,
                        top_p: request.top_p.unwrap_or(1.0),
                        truncation: "disabled".to_string(),
                        usage: tracked_usage.unwrap_or_else(|| Usage::new(0, 0)),
                        user: None,
                        metadata: request.metadata,
                    }
                };

                debug!(
                    "Created response {} for key {}",
                    response.id, api_key.api_key.created_by_user_id.0
                );

                // Attempt the existing gateway signature write before returning.
                // It stores only the response ID and signatures over
                // request/response digests, never the response body. Failures
                // and timeouts leave the no-store inference response unchanged.
                persist_non_streaming_response_attestation(
                    state.attestation_service.as_ref(),
                    &response,
                    body_hash.hash.clone(),
                )
                .await;

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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::StreamExt;
    use services::attestation::{
        ita::{ItaTokenQuery, ItaTokenResponse},
        AttestationError, SignatureLookupResult,
    };

    #[derive(Clone, Default)]
    struct RecordingAttestationService {
        stored_response_signatures: Arc<Mutex<Vec<(String, String, String)>>>,
    }

    impl RecordingAttestationService {
        fn stored_response_signatures(&self) -> Vec<(String, String, String)> {
            self.stored_response_signatures
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    #[async_trait]
    impl AttestationServiceTrait for RecordingAttestationService {
        async fn get_chat_signature(
            &self,
            _chat_id: &str,
            _signing_algo: Option<String>,
        ) -> Result<SignatureLookupResult, AttestationError> {
            Err(AttestationError::InternalError("unused".to_string()))
        }

        async fn store_chat_signature_from_provider(
            &self,
            _chat_id: &str,
        ) -> Result<(), AttestationError> {
            Ok(())
        }

        async fn store_chat_signature(
            &self,
            _chat_id: &str,
            _request_hash: String,
            _response_hash: String,
        ) -> Result<(), AttestationError> {
            Ok(())
        }

        async fn store_response_signature(
            &self,
            response_id: &str,
            request_hash: String,
            response_hash: String,
        ) -> Result<(), AttestationError> {
            self.stored_response_signatures
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((response_id.to_string(), request_hash, response_hash));
            Ok(())
        }

        async fn get_attestation_report(
            &self,
            _model: Option<String>,
            _signing_algo: Option<String>,
            _nonce: Option<String>,
            _signing_address: Option<String>,
            _include_tls_fingerprint: bool,
            _provider_filter: Option<inference_providers::ProviderTier>,
        ) -> Result<services::attestation::models::AttestationReport, AttestationError> {
            Err(AttestationError::InternalError("unused".to_string()))
        }

        async fn get_ita_attestation_token(
            &self,
            _query: ItaTokenQuery,
        ) -> Result<ItaTokenResponse, AttestationError> {
            Err(AttestationError::InternalError("unused".to_string()))
        }

        async fn verify_vpc_signature(
            &self,
            _timestamp: i64,
            _signature: String,
        ) -> Result<bool, AttestationError> {
            Ok(false)
        }
    }

    fn sample_response(response_id: &str) -> ResponseObject {
        ResponseObject {
            id: response_id.to_string(),
            object: "response".to_string(),
            created_at: 0,
            status: ResponseStatus::Completed,
            background: false,
            conversation: None,
            error: None,
            incomplete_details: None,
            instructions: None,
            max_output_tokens: None,
            max_tool_calls: None,
            model: "test-model".to_string(),
            output: vec![],
            parallel_tool_calls: false,
            previous_response_id: None,
            next_response_ids: vec![],
            prompt_cache_key: None,
            prompt_cache_retention: None,
            reasoning: None,
            safety_identifier: None,
            service_tier: "default".to_string(),
            store: false,
            temperature: 1.0,
            tool_choice: ResponseToolChoiceOutput::Auto("auto".to_string()),
            tools: vec![],
            top_logprobs: 0,
            top_p: 1.0,
            truncation: "disabled".to_string(),
            usage: Usage::new(0, 0),
            user: None,
            metadata: None,
        }
    }

    fn stream_event(event_type: &str, response: Option<ResponseObject>) -> ResponseStreamEvent {
        ResponseStreamEvent {
            event_type: event_type.to_string(),
            sequence_number: None,
            response,
            output_index: None,
            content_index: None,
            item: None,
            item_id: None,
            part: None,
            delta: None,
            text: None,
            error: None,
            status_code: None,
            logprobs: None,
            obfuscation: None,
            annotation_index: None,
            annotation: None,
            conversation_title: None,
            usage: None,
        }
    }

    #[tokio::test]
    async fn response_history_is_explicitly_gone() {
        let response = response_history_gone().await;
        assert_eq!(response.status(), StatusCode::GONE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        let error: ErrorResponse = serde_json::from_slice(&body).expect("gone error body");
        assert_eq!(error.error.r#type, "gone");
    }

    #[tokio::test]
    async fn streaming_response_stores_signature_before_completed_frame() {
        let attestation = Arc::new(RecordingAttestationService::default());
        let response_id = "resp_11111111-1111-4111-8111-111111111111";
        let events = vec![
            stream_event("response.created", Some(sample_response(response_id))),
            stream_event("response.completed", Some(sample_response(response_id))),
        ];
        let mut stream = signed_response_sse_stream(
            Box::pin(futures::stream::iter(events)),
            attestation.clone(),
            "request-digest".to_string(),
        );

        let created_frame = stream
            .next()
            .await
            .expect("created frame")
            .expect("infallible frame");
        assert!(attestation.stored_response_signatures().is_empty());

        let completed_frame = stream
            .next()
            .await
            .expect("completed frame")
            .expect("infallible frame");

        let signatures = attestation.stored_response_signatures();
        assert_eq!(signatures.len(), 1);
        let (stored_response_id, request_hash, response_hash) = &signatures[0];
        assert_eq!(stored_response_id, response_id);
        assert_eq!(request_hash, "request-digest");

        let mut expected_hasher = Sha256::new();
        expected_hasher.update(&created_frame);
        expected_hasher.update(&completed_frame);
        assert_eq!(response_hash, &hex::encode(expected_hasher.finalize()));
    }

    #[tokio::test]
    async fn client_disconnect_before_completion_does_not_store_partial_signature() {
        let attestation = Arc::new(RecordingAttestationService::default());
        let response_id = "resp_22222222-2222-4222-8222-222222222222";
        let mut stream = signed_response_sse_stream(
            Box::pin(futures::stream::iter(vec![
                stream_event("response.created", Some(sample_response(response_id))),
                stream_event("response.output_text.delta", None),
                stream_event("response.completed", Some(sample_response(response_id))),
            ])),
            attestation.clone(),
            "request-digest".to_string(),
        );

        let _created_frame = stream
            .next()
            .await
            .expect("created frame")
            .expect("infallible frame");
        drop(stream);

        assert!(attestation.stored_response_signatures().is_empty());
    }

    #[tokio::test]
    async fn non_streaming_response_stores_response_json_digest() {
        let attestation = RecordingAttestationService::default();
        let response = sample_response("resp_33333333-3333-4333-8333-333333333333");

        persist_non_streaming_response_attestation(
            &attestation,
            &response,
            "request-digest".to_string(),
        )
        .await;

        let signatures = attestation.stored_response_signatures();
        assert_eq!(signatures.len(), 1);
        let (stored_response_id, request_hash, response_hash) = &signatures[0];
        assert_eq!(stored_response_id, &response.id);
        assert_eq!(request_hash, "request-digest");
        let expected_hash = hex::encode(Sha256::digest(
            serde_json::to_vec(&response).expect("response serialization"),
        ));
        assert_eq!(response_hash, &expected_hash);
    }
}
