use crate::{
    middleware::{auth::AuthenticatedApiKey, RequestBodyHash, RequestCorrelation, RequestedModel},
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
use services::responses::client_tools::{
    contains_mcp_tool, normalize_request, ClientToolAdapter, MCP_TOOLS_UNSUPPORTED,
};
use services::responses::errors::ResponseError as ServiceResponseError;
use services::responses::models::*;
use services::responses::ports::ResponseServiceTrait;
use services::responses::service::ResponseServiceImpl;
use sha2::{Digest, Sha256};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;

#[path = "responses/client_tool_schema.rs"]
mod client_tool_schema;
pub use client_tool_schema::{StatelessResponseToolSchema, StatelessToolChoiceSchema};

/// Bound best-effort attestation persistence so a database problem never
/// prevents a completed inference response from being delivered.
const RESPONSE_ATTESTATION_STORE_TIMEOUT: Duration = Duration::from_secs(5);

/// OpenAPI view of the typed stateless Responses adapter for non-OpenAI models.
///
/// The runtime request type keeps legacy variants so it can return a precise
/// `invalid_request_error` for them. Both transports normalize omitted or null
/// `store` to `false`. Canonical `openai/` models use the provider's native
/// Responses transport, which preserves validated provider-native fields
/// outside this typed schema. The typed adapter also executes hosted web search
/// using request-scoped context. Native Responses accepts client-managed tools
/// and top-level `web_search`, executed upstream, and preserves complete
/// `web_search_call` replay items and `include` unchanged.
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
    pub tool_choice: Option<StatelessToolChoiceSchema>,
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

fn validate_legacy_encryption_headers(
    headers: &HeaderMap,
) -> Result<crate::routes::common::EncryptionHeaders, (StatusCode, ResponseJson<ErrorResponse>)> {
    let encryption = crate::routes::common::validate_encryption_headers(headers)?;
    if encryption.encryption_version.as_deref() == Some("3") {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "E2EE v3 is supported on /v1/chat/completions. The legacy /v1/responses tool flow does not support it.".to_string(),
                "unsupported_encryption_version".to_string(),
            )),
        ));
    }
    Ok(encryption)
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
    AdditionalTools {
        #[serde(rename = "type")]
        type_: AdditionalToolsType,
        /// Must be `developer`; declarations precede the conversation items.
        role: String,
        tools: Vec<StatelessResponseToolSchema>,
    },
    Reasoning {
        #[serde(rename = "type")]
        type_: ReasoningType,
        #[serde(default)]
        summary: Vec<ReasoningSummaryPart>,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ReasoningTextPart>>,
        /// Encrypted-only reasoning requires a native Responses model.
        #[serde(skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
    /// Completed search markers may accompany replayed assistant text.
    /// They are informational and do not trigger another search.
    WebSearchCall {
        #[serde(rename = "type")]
        type_: CompletedWebSearchCallType,
        status: CompletedWebSearchCallStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        action: Option<serde_json::Value>,
    },
    FunctionCall {
        #[serde(rename = "type")]
        type_: FunctionCallType,
        call_id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        arguments: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
    },
    FunctionCallOutput {
        #[serde(rename = "type")]
        type_: FunctionCallOutputType,
        call_id: String,
        output: StatelessToolOutputSchema,
    },
    CustomToolCall {
        #[serde(rename = "type")]
        type_: CustomToolCallType,
        call_id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        input: String,
    },
    CustomToolCallOutput {
        #[serde(rename = "type")]
        type_: CustomToolCallOutputType,
        call_id: String,
        output: StatelessToolOutputSchema,
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
pub enum CompletedWebSearchCallType {
    #[serde(rename = "web_search_call")]
    WebSearchCall,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
pub enum CompletedWebSearchCallStatus {
    #[serde(rename = "completed")]
    Completed,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
pub enum AdditionalToolsType {
    #[serde(rename = "additional_tools")]
    AdditionalTools,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
pub enum CustomToolCallType {
    #[serde(rename = "custom_tool_call")]
    CustomToolCall,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
pub enum CustomToolCallOutputType {
    #[serde(rename = "custom_tool_call_output")]
    CustomToolCallOutput,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum StatelessToolOutputSchema {
    Text(String),
    Parts(Vec<StatelessToolOutputTextSchema>),
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(tag = "type")]
pub enum StatelessToolOutputTextSchema {
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(rename = "output_text")]
    OutputText { text: String },
    #[serde(rename = "text")]
    Text { text: String },
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
    pub native_service: services::responses::native::NativeResponsesService,
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
        event: &serde_json::Value,
        sse_bytes: &[u8],
    ) -> Option<(String, String)> {
        if self.response_id.is_none() {
            self.response_id = event
                .pointer("/response/id")
                .and_then(|id| id.as_str())
                .map(str::to_owned);
        }

        self.response_hasher.update(sse_bytes);

        if self.completed || event["type"] != "response.completed" {
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
    response_id: &str,
    response_bytes: &[u8],
    request_hash: String,
) {
    let response_hash = hex::encode(Sha256::digest(response_bytes));
    persist_response_attestation(
        attestation_service,
        response_id,
        request_hash,
        response_hash,
    )
    .await;
}

async fn signed_non_streaming_response(
    response: ResponseObject,
    adapter: &ClientToolAdapter,
    attestation_service: &dyn AttestationServiceTrait,
    request_hash: String,
) -> axum::response::Response {
    let response_json = match serde_json::to_value(&response)
        .map_err(|_| ())
        .and_then(|value| adapter.response(value).map_err(|_| ()))
    {
        Ok(value) => value,
        Err(()) => {
            tracing::error!("Response client-tool serialization failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to serialize the response's client tools".to_string(),
                    "server_error".to_string(),
                )),
            )
                .into_response();
        }
    };
    // Serialize once: both the attestation and HTTP body use these exact bytes.
    let response_bytes = response_json.to_string().into_bytes();
    persist_non_streaming_response_attestation(
        attestation_service,
        &response.id,
        &response_bytes,
        request_hash,
    )
    .await;
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        response_bytes,
    )
        .into_response()
}

fn client_tool_serialization_failure(
    event: &ResponseStreamEvent,
    response_id: Option<&str>,
) -> serde_json::Value {
    let response = event.response.as_ref();
    let response_id = response
        .map(|response| response.id.as_str())
        .or(response_id)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("resp_{}", uuid::Uuid::new_v4().simple()));
    let error = serde_json::json!({
        "type": "server_error",
        "code": "server_error",
        "message": "Failed to serialize the response's client tools",
        "param": null,
    });
    // Do not expose a malformed internal wrapper or retain transcript content
    // to recover an invariant failure. The service validates calls beforehand.
    serde_json::json!({
        "type": "response.failed",
        "sequence_number": event.sequence_number.unwrap_or(0),
        "response": {
            "id": response_id,
            "object": "response",
            "created_at": response.map(|response| response.created_at).unwrap_or_else(|| chrono::Utc::now().timestamp()),
            "model": response.map(|response| response.model.as_str()),
            "status": "failed",
            "error": error,
            "output": [],
            "usage": event.usage.as_ref().or_else(|| response.map(|response| &response.usage)),
            "incomplete_details": null,
            "store": false,
        },
        "error": error,
        "status_code": 500,
    })
}

/// Convert response events into signed SSE frames. The response-completed
/// frame is held until its best-effort attestation write has been attempted.
/// A successful write is available for subsequent `resp_*` lookup; a failed or
/// timed-out write does not change the inference result.
fn signed_response_sse_stream(
    stream: Pin<Box<dyn futures::Stream<Item = ResponseStreamEvent> + Send>>,
    adapter: ClientToolAdapter,
    attestation_service: Arc<dyn AttestationServiceTrait>,
    request_hash: String,
) -> Pin<Box<dyn futures::Stream<Item = Result<Bytes, Infallible>> + Send>> {
    let state = (
        stream,
        adapter.stream(),
        StreamingResponseAttestation::default(),
        false,
    );
    Box::pin(futures::stream::unfold(
        state,
        move |(mut stream, mut adapter, mut signature_state, ended)| {
            let attestation_service = attestation_service.clone();
            let request_hash = request_hash.clone();
            async move {
                if ended {
                    return None;
                }
                let event = stream.next().await?;
                let (event, ended) = match serde_json::to_value(&event)
                    .map_err(|_| ())
                    .and_then(|value| adapter.event(value).map_err(|_| ()))
                {
                    Ok(value) => (value, false),
                    Err(()) => {
                        tracing::error!("Response stream client-tool serialization failed");
                        (
                            client_tool_serialization_failure(
                                &event,
                                signature_state.response_id.as_deref(),
                            ),
                            true,
                        )
                    }
                };
                let event_type = event["type"].as_str().unwrap_or("response.failed");
                let sse_bytes = format!("event: {event_type}\ndata: {event}\n\n");
                let signature = signature_state.record_event(&event, sse_bytes.as_bytes());

                if let Some((response_id, response_hash)) = signature {
                    persist_response_attestation(
                        attestation_service.as_ref(),
                        &response_id,
                        request_hash,
                        response_hash,
                    )
                    .await;
                }

                Some((
                    Ok::<Bytes, Infallible>(Bytes::from(sse_bytes)),
                    (stream, adapter, signature_state, ended),
                ))
            }
        },
    ))
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
///
/// Canonical `openai/` models use one upstream native Responses request, with
/// top-level `web_search` executed by the provider. Complete `web_search_call`
/// replay items and `include` are forwarded unchanged.
/// Other models use the typed adapter, which can execute `web_search` and
/// `web_context_search` in a bounded, request-scoped inference loop. Both paths
/// normalize omitted or null `store` to `false` and keep no response/item history.
/// Client-managed tools are returned to the caller. MCP tools are no longer
/// supported and return 400.
#[utoipa::path(
    post,
    path = "/v1/responses",
    tag = "Responses",
    request_body(
        content = serde_json::Value,
        description = "Polymorphic stateless JSON request. After alias resolution, canonical openai/ models accept validated provider-native Responses fields. Other models use the typed StatelessCreateResponseRequestSchema component. Both paths normalize omitted or null store to false and reject store: true. The typed adapter supports hosted web_search and web_context_search with request-scoped context. Native Responses accepts client-managed tools and top-level web_search, executed upstream in one provider request; complete web_search_call replay items and include are forwarded unchanged. File tools are unsupported. MCP tools are no longer supported and return 400 with param tools."
    ),
    responses(
        (
            status = 200,
            description = "Polymorphic success response. For non-OpenAI models, the typed adapter returns ResponseObject metadata with hosted search events/citations, client-managed function/custom call items, and optional namespaces as JSON for non-streaming requests. Canonical openai/ models forward the provider-native response shape, including native reasoning, web_search_call items, and search citations or sources returned by the provider. Streaming requests use text/event-stream on either path.",
            content(
                (serde_json::Value = "application/json"),
                ("text/event-stream")
            )
        ),
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
    requested_model: RequestedModel,
    OpenAiJson(raw): OpenAiJson<Box<serde_json::value::RawValue>>,
) -> axum::response::Response {
    let body = match axum::Json::<serde_json::Value>::from_bytes(raw.get().as_bytes()) {
        Ok(axum::Json(body)) => body,
        Err(error) => return super::extractors::OpenAiJsonRejection(error).into_response(),
    };
    if let Some(model) = body.get("model").and_then(|model| model.as_str()) {
        requested_model.set(model);
    }
    // MCP tools are deprecated. Reject them on every transport before model
    // resolution or any provider work.
    if contains_mcp_tool(&body) {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::with_param(
                MCP_TOOLS_UNSUPPORTED.to_string(),
                "invalid_request_error".to_string(),
                "tools".to_string(),
            )),
        )
            .into_response();
    }
    let native = match state
        .native_service
        .selected_model(
            &body,
            services::models::ModelCaller::Organization(api_key.organization.id.0),
        )
        .await
    {
        Ok(model) => model,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to resolve model".to_string(),
                    "api_error".to_string(),
                )),
            )
                .into_response();
        }
    };
    if let Some(model) = native {
        return super::responses_native::handle(
            state.native_service,
            model,
            api_key,
            headers,
            body_hash.hash,
            body,
        )
        .await;
    }
    // All other requests use the typed stateless adapter and its validation.
    // Native-only fields never pass through that conversion.
    let request = match typed_response_request(body) {
        Ok(request) => request,
        Err(response) => return *response,
    };
    create_legacy_response(
        State(state),
        Extension(api_key),
        Extension(body_hash),
        Extension(correlation),
        headers,
        OpenAiJson(request),
    )
    .await
}

fn typed_response_request(
    body: serde_json::Value,
) -> Result<CreateResponseRequest, Box<axum::response::Response>> {
    let (body, adapter) = normalize_request(body).map_err(|message| {
        Box::new(
            (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    message,
                    "invalid_request_error".to_string(),
                )),
            )
                .into_response(),
        )
    })?;
    let axum::Json(mut request) = axum::Json::<CreateResponseRequest>::from_bytes(
        body.to_string().as_bytes(),
    )
    .map_err(|error| Box::new(super::extractors::OpenAiJsonRejection(error).into_response()))?;
    request.client_tool_adapter = adapter;
    Ok(request)
}

async fn create_legacy_response(
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
    let encryption_headers = match validate_legacy_encryption_headers(&headers) {
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
    let client_tool_adapter = request.client_tool_adapter.clone();

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
                api_key.organization.fallback_enabled(),
                api_key.organization.request_priority,
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
                    client_tool_adapter,
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
                    status_code = error.http_status_code(),
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
                api_key.organization.fallback_enabled(),
                api_key.organization.request_priority,
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
                            failed_error = event
                                .response
                                .as_ref()
                                .and_then(|response| response.error.clone())
                                .or_else(|| event.error.clone());
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
                signed_non_streaming_response(
                    response,
                    &client_tool_adapter,
                    state.attestation_service.as_ref(),
                    body_hash.hash.clone(),
                )
                .await
            }
            Err(error) => {
                tracing::error!(
                    user_id = %api_key.api_key.created_by_user_id.0,
                    model = %model,
                    status_code = error.http_status_code(),
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
    use std::sync::Mutex;

    #[test]
    fn legacy_responses_rejects_v3_and_preserves_older_versions() {
        for version in [None, Some("1"), Some("2"), Some("3")] {
            let mut headers = axum::http::HeaderMap::new();
            if let Some(version) = version {
                headers.insert("x-encryption-version", version.parse().unwrap());
            }
            let result = validate_legacy_encryption_headers(&headers);
            if version == Some("3") {
                let (status, _) = result.expect_err("v3 must be rejected at the route");
                assert_eq!(status, StatusCode::BAD_REQUEST);
            } else {
                assert!(result.is_ok());
            }
        }
    }

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
            _provider_filter: Option<inference_providers::ProviderFilter>,
            _caller: services::models::ModelCaller,
        ) -> Result<services::attestation::models::AttestationReport, AttestationError> {
            Err(AttestationError::InternalError("unused".to_string()))
        }

        async fn get_ita_attestation_token(
            &self,
            _query: ItaTokenQuery,
        ) -> Result<ItaTokenResponse, AttestationError> {
            Err(AttestationError::InternalError("unused".to_string()))
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
            arguments: None,
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
    async fn failed_stream_preserves_error_envelope_without_storing_completed_attestation() {
        let attestation_service = Arc::new(RecordingAttestationService::default());
        let mut response = sample_response("resp_failed_test");
        response.status = ResponseStatus::Failed;
        response.error = Some(services::responses::models::ResponseError {
            message: "Request rejected by provider".to_string(),
            type_: "invalid_request_error".to_string(),
            param: None,
            code: Some("invalid_prompt".to_string()),
        });
        let failed = stream_event("response.failed", Some(response));
        let mut stream = signed_response_sse_stream(
            Box::pin(futures::stream::iter([failed])),
            ClientToolAdapter::default(),
            attestation_service.clone(),
            "request-hash".to_string(),
        );
        let frame = stream.next().await.unwrap().unwrap();
        let text = std::str::from_utf8(&frame).unwrap();
        let data = text
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap();
        let event: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(event["type"], "response.failed");
        assert_eq!(event["response"]["id"], "resp_failed_test");
        assert_eq!(
            event["response"]["error"]["message"],
            "Request rejected by provider"
        );
        assert!(attestation_service.stored_response_signatures().is_empty());
        assert!(stream.next().await.is_none());
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
            ClientToolAdapter::default(),
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
            ClientToolAdapter::default(),
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

        let http_response = signed_non_streaming_response(
            response.clone(),
            &ClientToolAdapter::default(),
            &attestation,
            "request-digest".to_string(),
        )
        .await;
        assert_eq!(http_response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(http_response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::to_value(&response).unwrap()
        );

        let signatures = attestation.stored_response_signatures();
        assert_eq!(signatures.len(), 1);
        let (stored_response_id, request_hash, response_hash) = &signatures[0];
        assert_eq!(stored_response_id, &response.id);
        assert_eq!(request_hash, "request-digest");
        let expected_hash = hex::encode(Sha256::digest(&body));
        assert_eq!(response_hash, &expected_hash);
    }

    fn custom_tool_request() -> (CreateResponseRequest, String) {
        let request = typed_response_request(serde_json::json!({
            "model": "test-model",
            "tools": [{"type":"namespace", "name":"workspace", "tools":[
                {"type":"custom", "name":"apply_patch", "format":{"type":"text"}}
            ]}],
            "tool_choice": {"type":"custom", "namespace":"workspace", "name":"apply_patch"},
            "input": "Apply the patch"
        }))
        .unwrap();
        let tools = serde_json::to_value(&request.tools).unwrap();
        let name = tools[0]["name"].as_str().unwrap().to_string();
        (request, name)
    }

    fn custom_function_item(name: &str, arguments: &str, status: &str) -> ResponseOutputItem {
        serde_json::from_value(serde_json::json!({
            "type":"function_call", "id":"fc_custom", "response_id":"resp_custom",
            "call_id":"call_custom", "name":name, "arguments":arguments,
            "status":status, "model":"test-model"
        }))
        .unwrap()
    }

    fn frame_json(frame: &Bytes) -> serde_json::Value {
        let text = std::str::from_utf8(frame).unwrap();
        let data = text
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap();
        serde_json::from_str(data).unwrap()
    }

    #[test]
    fn typed_route_normalizes_inline_custom_tools_and_replay_before_deserialization() {
        let request = typed_response_request(serde_json::json!({
            "model":"test-model",
            "input":[
                {"type":"additional_tools", "role":"developer", "tools":[
                    {"type":"custom", "name":"exec", "format":{"type":"text"}}
                ]},
                {"type":"custom_tool_call", "call_id":"call_exec", "name":"exec", "input":"print(1)"},
                {"type":"custom_tool_call_output", "call_id":"call_exec", "output":[{"type":"input_text", "text":"1"}]},
                {"role":"user", "content":"Continue"}
            ]
        })).unwrap();
        request.validate_stateless().unwrap();
        let input = serde_json::to_value(&request.input).unwrap();
        let tools = serde_json::to_value(&request.tools).unwrap();
        assert_eq!(input.as_array().unwrap().len(), 3);
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["name"], tools[0]["name"]);
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["output"], "1");
    }

    #[tokio::test]
    async fn streaming_custom_tool_attestation_hashes_transformed_headers_and_data() {
        let (request, name) = custom_tool_request();
        let input = "*** Begin Patch\n*** End Patch\n";
        let arguments = serde_json::json!({"input":input}).to_string();
        let mut completed = sample_response("resp_custom");
        completed.tools = request.tools.clone().unwrap();
        completed.output = vec![custom_function_item(&name, &arguments, "completed")];
        let mut added = stream_event("response.output_item.added", None);
        added.item = Some(custom_function_item(&name, "", "in_progress"));
        added.output_index = Some(0);
        let mut delta = stream_event("response.function_call_arguments.delta", None);
        delta.item_id = Some("fc_custom".to_string());
        delta.output_index = Some(0);
        delta.delta = Some(arguments.clone());
        let mut done = stream_event("response.function_call_arguments.done", None);
        done.item_id = Some("fc_custom".to_string());
        done.output_index = Some(0);
        done.arguments = Some(arguments);
        let mut item_done = stream_event("response.output_item.done", None);
        item_done.item = completed.output.first().cloned();
        item_done.output_index = Some(0);
        let events = vec![
            stream_event("response.created", Some(sample_response("resp_custom"))),
            added,
            delta,
            done,
            item_done,
            stream_event("response.completed", Some(completed)),
        ];
        let attestation = Arc::new(RecordingAttestationService::default());
        let mut stream = signed_response_sse_stream(
            Box::pin(futures::stream::iter(events)),
            request.client_tool_adapter,
            attestation.clone(),
            "custom-request-digest".to_string(),
        );
        let mut frames = Vec::new();
        while let Some(frame) = stream.next().await {
            frames.push(frame.unwrap());
            assert_eq!(
                attestation.stored_response_signatures().len(),
                usize::from(frames.len() == 6)
            );
        }
        assert_eq!(frames.len(), 6);
        let added = frame_json(&frames[1]);
        assert_eq!(added["item"]["type"], "custom_tool_call");
        assert_eq!(added["item"]["namespace"], "workspace");
        assert_eq!(added["item"]["name"], "apply_patch");
        assert_eq!(added["item"]["input"], "");
        assert!(std::str::from_utf8(&frames[2])
            .unwrap()
            .starts_with("event: response.custom_tool_call_input.delta\n"));
        assert_eq!(frame_json(&frames[2])["delta"], input);
        assert!(std::str::from_utf8(&frames[3])
            .unwrap()
            .starts_with("event: response.custom_tool_call_input.done\n"));
        assert_eq!(frame_json(&frames[3])["input"], input);
        assert_eq!(frame_json(&frames[4])["item"]["input"], input);
        let final_response = frame_json(&frames[5]);
        assert_eq!(final_response["response"]["output"][0]["input"], input);
        assert_eq!(final_response["response"]["tools"][0]["type"], "namespace");
        let actual_bytes: Vec<u8> = frames
            .iter()
            .flat_map(|frame| frame.iter().copied())
            .collect();
        assert_eq!(
            attestation.stored_response_signatures(),
            vec![(
                "resp_custom".to_string(),
                "custom-request-digest".to_string(),
                hex::encode(Sha256::digest(actual_bytes))
            )]
        );
    }

    #[tokio::test]
    async fn non_streaming_custom_tool_attestation_matches_returned_json_bytes() {
        let (request, name) = custom_tool_request();
        let mut response = sample_response("resp_custom");
        response.tools = request.tools.unwrap();
        response.output = vec![custom_function_item(
            &name,
            r#"{"input":"patch"}"#,
            "completed",
        )];
        let attestation = RecordingAttestationService::default();
        let http_response = signed_non_streaming_response(
            response,
            &request.client_tool_adapter,
            &attestation,
            "custom-request-digest".to_string(),
        )
        .await;
        assert_eq!(http_response.status(), StatusCode::OK);
        assert_eq!(
            http_response.headers()[header::CONTENT_TYPE],
            "application/json"
        );
        let bytes = axum::body::to_bytes(http_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["output"][0]["type"], "custom_tool_call");
        assert_eq!(body["output"][0]["namespace"], "workspace");
        assert_eq!(body["output"][0]["name"], "apply_patch");
        assert_eq!(body["output"][0]["input"], "patch");
        assert!(body["output"][0].get("arguments").is_none());
        assert_eq!(body["tools"][0]["type"], "namespace");
        assert_eq!(
            attestation.stored_response_signatures(),
            vec![(
                "resp_custom".to_string(),
                "custom-request-digest".to_string(),
                hex::encode(Sha256::digest(&bytes))
            )]
        );
    }

    #[tokio::test]
    async fn malformed_custom_wrapper_fails_closed_without_completed_attestation() {
        let (request, name) = custom_tool_request();
        let mut response = sample_response("resp_custom");
        response.output = vec![custom_function_item(
            &name,
            "malformed wrapper",
            "completed",
        )];
        response.usage = Usage::new(7, 3);
        let mut malformed = stream_event("response.completed", Some(response.clone()));
        malformed.sequence_number = Some(2);
        let attestation = Arc::new(RecordingAttestationService::default());
        let mut stream = signed_response_sse_stream(
            Box::pin(futures::stream::iter([
                stream_event("response.created", Some(sample_response("resp_custom"))),
                malformed,
                stream_event("response.completed", Some(sample_response("resp_custom"))),
            ])),
            request.client_tool_adapter.clone(),
            attestation.clone(),
            "request-digest".to_string(),
        );
        stream.next().await.unwrap().unwrap();
        let failure = stream.next().await.unwrap().unwrap();
        assert!(std::str::from_utf8(&failure)
            .unwrap()
            .starts_with("event: response.failed\n"));
        let event = frame_json(&failure);
        assert_eq!(event["sequence_number"], 2);
        assert_eq!(event["response"]["id"], "resp_custom");
        assert_eq!(event["response"]["status"], "failed");
        assert_eq!(event["response"]["error"]["code"], "server_error");
        assert_eq!(event["response"]["usage"]["total_tokens"], 10);
        assert_eq!(event["response"]["output"], serde_json::json!([]));
        assert!(stream.next().await.is_none());
        assert!(attestation.stored_response_signatures().is_empty());

        let http_response = signed_non_streaming_response(
            response,
            &request.client_tool_adapter,
            attestation.as_ref(),
            "request-digest".to_string(),
        )
        .await;
        assert_eq!(http_response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(http_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.error.r#type, "server_error");
        assert!(!error.error.message.contains("malformed wrapper"));
        assert!(attestation.stored_response_signatures().is_empty());
    }
}
