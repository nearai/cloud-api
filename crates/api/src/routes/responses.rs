use super::completions::HEADER_INFERENCE_ID;
use crate::{
    middleware::{auth::AuthenticatedApiKey, RequestBodyHash, RequestCorrelation, RequestedModel},
    models::ErrorResponse,
    routes::extractors::OpenAiJson,
};
use axum::{
    body::Body,
    extract::{Extension, State},
    http::{header, HeaderMap, HeaderValue, Response, StatusCode},
    response::{IntoResponse, Json as ResponseJson},
};
use bytes::Bytes;
use futures::stream::StreamExt;
use services::attestation::{ports::AttestationServiceTrait, SignatureOwner};
use services::metrics::{
    consts::{
        get_environment, METRIC_RESPONSES_USAGE_CONFIRMATION,
        METRIC_RESPONSES_USAGE_CONFIRMATION_DURATION, TAG_ENVIRONMENT, TAG_REASON, TAG_RESULT,
    },
    MetricsServiceTrait,
};
use services::responses::billing::ResponseBilling;
use services::responses::client_tools::{
    contains_mcp_tool, normalize_request, ClientToolAdapter, MCP_TOOLS_UNSUPPORTED,
};
use services::responses::errors::ResponseError as ServiceResponseError;
use services::responses::models::*;
use services::responses::service::{ResponseEventStream, ResponseServiceImpl};
use services::usage::UsageServiceTrait;
use sha2::{Digest, Sha256};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

#[path = "responses/client_tool_schema.rs"]
mod client_tool_schema;
pub use client_tool_schema::{StatelessResponseToolSchema, StatelessToolChoiceSchema};

/// Bound best-effort attestation persistence so a database problem never
/// prevents a completed inference response from being delivered.
const RESPONSE_ATTESTATION_STORE_TIMEOUT: Duration = Duration::from_secs(5);

/// The completion writer can remain live for 30 seconds. Wait beyond that
/// hard cap before declaring a completed response unbilled.
const RESPONSE_USAGE_CONFIRM_TIMEOUT: Duration = Duration::from_secs(35);
const RESPONSE_USAGE_CONFIRM_FIRST_DELAY: Duration = Duration::from_millis(20);
const RESPONSE_USAGE_CONFIRM_MAX_DELAY: Duration = Duration::from_millis(250);
const USAGE_NOT_RECORDED_MESSAGE: &str = "Failed to record usage";

/// Bound the wait for the first inference round's identifier before a
/// streamed response starts. The wait ends at the first provider chunk.
const STREAM_INFERENCE_ID_PEEK_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound the events held while waiting for that identifier.
const MAX_STREAM_INFERENCE_ID_PEEK_EVENTS: usize = 32;
const HEADER_INFERENCE_IDS: &str = "Inference-Ids";

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
    pub metrics_service: Arc<dyn MetricsServiceTrait>,
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
    owner: SignatureOwner,
    response_id: &str,
    request_hash: String,
    response_hash: String,
) {
    match tokio::time::timeout(
        RESPONSE_ATTESTATION_STORE_TIMEOUT,
        attestation_service.store_response_signature(
            owner,
            response_id,
            request_hash,
            response_hash,
        ),
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
    owner: SignatureOwner,
    response_id: &str,
    response_bytes: &[u8],
    request_hash: String,
) {
    let response_hash = hex::encode(Sha256::digest(response_bytes));
    persist_response_attestation(
        attestation_service,
        owner,
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
    owner: SignatureOwner,
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
        owner,
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

/// The billed inference rounds of a response could not be confirmed.
#[derive(Debug)]
struct UsageNotConfirmed;

/// Confirms that the completion layer recorded a usage row for every billed
/// inference round of one typed response.
#[derive(Clone)]
struct UsageConfirmation {
    usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
    organization_id: Uuid,
    billing: ResponseBilling,
    metrics_service: Arc<dyn MetricsServiceTrait>,
}

impl UsageConfirmation {
    /// Return the billed inference IDs once each has a usage row, and record
    /// the outcome and time taken. Tags are constants and the environment only.
    async fn confirm(&self) -> Result<Vec<Uuid>, UsageNotConfirmed> {
        let started = tokio::time::Instant::now();
        let outcome = self.poll_usage_rows().await;
        let mut tags = vec![format!("{TAG_ENVIRONMENT}:{}", get_environment())];
        match &outcome {
            Ok(_) => tags.push(format!("{TAG_RESULT}:confirmed")),
            Err(failure_kind) => {
                tags.push(format!("{TAG_RESULT}:failed"));
                tags.push(format!("{TAG_REASON}:{failure_kind}"));
            }
        }
        let tags: Vec<&str> = tags.iter().map(String::as_str).collect();
        self.metrics_service
            .record_count(METRIC_RESPONSES_USAGE_CONFIRMATION, 1, &tags);
        self.metrics_service.record_latency(
            METRIC_RESPONSES_USAGE_CONFIRMATION_DURATION,
            started.elapsed(),
            &tags,
        );
        outcome.map_err(|_| UsageNotConfirmed)
    }

    /// Rows are written when each round's provider stream ends, so the lookup
    /// is repeated with backoff until a bounded deadline. A failure returns
    /// its constant failure kind.
    async fn poll_usage_rows(&self) -> Result<Vec<Uuid>, &'static str> {
        let organization_id = self.organization_id;
        let Ok(inference_ids) = self.billing.billed_inference_ids() else {
            tracing::error!(
                %organization_id,
                failure_kind = "unreported_usage",
                "Responses usage was not confirmed"
            );
            return Err("unreported_usage");
        };
        let deadline = tokio::time::Instant::now() + RESPONSE_USAGE_CONFIRM_TIMEOUT;
        let mut delay = RESPONSE_USAGE_CONFIRM_FIRST_DELAY;
        let mut found = 0usize;
        let failure_kind = loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let failure_kind = match tokio::time::timeout(
                remaining,
                self.usage_service
                    .get_costs_by_inference_ids(organization_id, inference_ids.clone()),
            )
            .await
            {
                Ok(Ok(costs)) => {
                    found = inference_ids
                        .iter()
                        .filter(|id| costs.iter().any(|cost| cost.inference_id == **id))
                        .count();
                    if found == inference_ids.len() {
                        return Ok(inference_ids);
                    }
                    "missing_rows"
                }
                Ok(Err(_)) => "lookup_error",
                Err(_) => "timeout",
            };
            if tokio::time::Instant::now() + delay >= deadline {
                break failure_kind;
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(RESPONSE_USAGE_CONFIRM_MAX_DELAY);
        };
        tracing::error!(
            %organization_id,
            expected = inference_ids.len(),
            found,
            failure_kind,
            "Responses usage was not confirmed"
        );
        Err(failure_kind)
    }
}

fn usage_not_recorded_response() -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        ResponseJson(ErrorResponse::new(
            USAGE_NOT_RECORDED_MESSAGE.to_string(),
            "server_error".to_string(),
        )),
    )
        .into_response()
}

/// Keep `Inference-Id` as one UUID for existing clients. A completed
/// multi-round JSON response also exposes every usage-row ID in order.
fn insert_inference_id_header(headers: &mut HeaderMap, inference_ids: &[Uuid]) {
    let Some(first_id) = inference_ids.first() else {
        return;
    };
    if let Ok(value) = HeaderValue::from_str(&first_id.to_string()) {
        headers.insert(HEADER_INFERENCE_ID, value);
    }
    if inference_ids.len() > 1 {
        let all_ids = inference_ids
            .iter()
            .map(Uuid::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        if let Ok(value) = HeaderValue::from_str(&all_ids) {
            headers.insert(HEADER_INFERENCE_IDS, value);
        }
        headers.insert(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_static("Inference-Id, Inference-Ids"),
        );
    } else {
        headers.insert(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_static(HEADER_INFERENCE_ID),
        );
    }
}

/// Collect a typed response stream for a non-streaming request. Only a
/// `response.completed` event yields a response; a failed or unterminated
/// stream becomes an error, so partial output is never returned as a success.
async fn collect_terminal_response(
    mut stream: ResponseEventStream,
) -> Result<ResponseObject, Box<axum::response::Response>> {
    let mut event_count = 0usize;
    let mut delta_count = 0usize;
    let mut delta_bytes = 0usize;
    let mut completed: Option<ResponseObject> = None;
    let mut failed: Option<(
        Option<services::responses::models::ResponseError>,
        Option<u16>,
    )> = None;
    while let Some(event) = stream.next().await {
        event_count += 1;
        match event.event_type.as_str() {
            "response.output_text.delta" => {
                if let Some(delta) = &event.delta {
                    delta_count += 1;
                    delta_bytes += delta.len();
                }
            }
            "response.completed" => {
                completed = event.response;
            }
            "response.failed" => {
                let error = event
                    .response
                    .as_ref()
                    .and_then(|response| response.error.clone())
                    .or_else(|| event.error.clone());
                failed = Some((error, event.status_code));
            }
            _ => {}
        }
    }
    tracing::debug!(
        event_count,
        delta_count,
        delta_bytes,
        completed = completed.is_some(),
        failed = failed.is_some(),
        "Non-streaming response collection finished"
    );

    if let Some(response) = completed {
        return Ok(response);
    }
    let response = match failed {
        Some((Some(error), status_code)) => (
            status_code_from_response_event(status_code),
            ResponseJson(error_response_from_response_event(error)),
        )
            .into_response(),
        Some((None, status_code)) => (
            status_code_from_response_event(status_code),
            ResponseJson(ErrorResponse::new(
                "Response failed".to_string(),
                "server_error".to_string(),
            )),
        )
            .into_response(),
        None => {
            tracing::error!(
                event_count,
                "Response stream ended without a terminal event"
            );
            let error = ServiceResponseError::StreamInterrupted;
            (
                map_response_error_to_status(&error),
                ResponseJson::<ErrorResponse>(error.into()),
            )
                .into_response()
        }
    };
    Err(Box::new(response))
}

/// Confirm usage, then sign and return a completed non-streaming response.
/// A response whose usage cannot be confirmed is not returned as a success.
async fn billed_non_streaming_response(
    response: ResponseObject,
    adapter: &ClientToolAdapter,
    attestation_service: &dyn AttestationServiceTrait,
    owner: SignatureOwner,
    request_hash: String,
    usage: &UsageConfirmation,
) -> axum::response::Response {
    let Ok(inference_ids) = usage.confirm().await else {
        return usage_not_recorded_response();
    };
    let mut http_response =
        signed_non_streaming_response(response, adapter, attestation_service, owner, request_hash)
            .await;
    if http_response.status() == StatusCode::OK {
        insert_inference_id_header(http_response.headers_mut(), &inference_ids);
    }
    http_response
}

/// Hold the leading events of a streamed response until its first inference
/// round reveals the identifier of its usage row, so the response headers can
/// carry it as Chat Completions does. The wait ends at the first provider
/// chunk, a terminal event, or a time or event-count bound; the held events
/// are returned in order for replay.
async fn peek_first_inference_id<S>(
    stream: &mut S,
    billing: &ResponseBilling,
) -> (Vec<ResponseStreamEvent>, Option<Uuid>)
where
    S: futures::Stream<Item = ResponseStreamEvent> + Unpin,
{
    let mut prelude = Vec::new();
    let _ = tokio::time::timeout(STREAM_INFERENCE_ID_PEEK_TIMEOUT, async {
        while billing.first_inference_id().is_none()
            && prelude.len() < MAX_STREAM_INFERENCE_ID_PEEK_EVENTS
        {
            let Some(event) = stream.next().await else {
                break;
            };
            let terminal = matches!(
                event.event_type.as_str(),
                "response.completed" | "response.failed"
            );
            prelude.push(event);
            if terminal {
                break;
            }
        }
    })
    .await;
    (prelude, billing.first_inference_id())
}

/// Build a complete `response.failed` event that replaces a frame which cannot
/// be delivered as-is. It takes the replaced frame's sequence number and keeps
/// only safe fields: the response id, creation time, model name, and token
/// counts. Output, request parameters, metadata, and upstream error text are
/// never copied.
fn stream_failure_event(
    event: &ResponseStreamEvent,
    response_id: Option<&str>,
    message: &str,
) -> serde_json::Value {
    let response = event.response.as_ref();
    let response_id = response
        .map(|response| response.id.as_str())
        .or(response_id)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("resp_{}", uuid::Uuid::new_v4().simple()));
    let usage = event
        .usage
        .as_ref()
        .or_else(|| response.map(|response| &response.usage));
    let (input_tokens, output_tokens, total_tokens) = usage.map_or((0, 0, 0), |usage| {
        (usage.input_tokens, usage.output_tokens, usage.total_tokens)
    });
    let error = serde_json::json!({
        "type": "server_error",
        "code": "server_error",
        "message": message,
        "param": null,
    });
    let mut failed = serde_json::json!({
        "type": "response.failed",
        "response": {
            "id": response_id,
            "object": "response",
            "created_at": response
                .map(|response| response.created_at)
                .unwrap_or_else(|| chrono::Utc::now().timestamp()),
            "status": "failed",
            "background": false,
            "error": error,
            "incomplete_details": null,
            "instructions": null,
            "max_output_tokens": null,
            "max_tool_calls": null,
            "model": response.map_or("", |response| response.model.as_str()),
            "output": [],
            "parallel_tool_calls": true,
            "previous_response_id": null,
            "reasoning": null,
            "service_tier": "default",
            "store": false,
            "temperature": 1.0,
            "tool_choice": "auto",
            "tools": [],
            "top_logprobs": 0,
            "top_p": 1.0,
            "truncation": "disabled",
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "total_tokens": total_tokens,
            },
            "user": null,
            "metadata": {},
        },
        "error": error,
        "status_code": 500,
    });
    if let Some(sequence) = event.sequence_number {
        failed["sequence_number"] = serde_json::json!(sequence);
    }
    failed
}

/// Convert response events into signed SSE frames. The response-completed
/// frame is held until usage for every billed inference round is confirmed
/// and its best-effort attestation write has been attempted. When usage
/// cannot be confirmed, a `response.failed` frame replaces it and ends the
/// stream. A successful attestation write is available for subsequent
/// `resp_*` lookup; a failed or timed-out write does not change the result.
fn signed_response_sse_stream(
    stream: ResponseEventStream,
    adapter: ClientToolAdapter,
    attestation_service: Arc<dyn AttestationServiceTrait>,
    owner: SignatureOwner,
    request_hash: String,
    usage: Option<UsageConfirmation>,
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
            let usage = usage.clone();
            async move {
                if ended {
                    return None;
                }
                let source = stream.next().await?;
                let (mut event, mut ended) = match serde_json::to_value(&source)
                    .map_err(|_| ())
                    .and_then(|value| adapter.event(value).map_err(|_| ()))
                {
                    Ok(value) => (value, false),
                    Err(()) => {
                        tracing::error!("Response stream client-tool serialization failed");
                        (
                            stream_failure_event(
                                &source,
                                signature_state.response_id.as_deref(),
                                "Failed to serialize the response's client tools",
                            ),
                            true,
                        )
                    }
                };
                if let Some(usage) = &usage {
                    if event["type"] == "response.completed" {
                        match usage.confirm().await {
                            Ok(inference_ids) => {
                                // The first ID is available in the response header;
                                // later hosted-search rounds are disclosed at completion.
                                event["inference_ids"] = serde_json::json!(inference_ids);
                            }
                            Err(_) => {
                                event = stream_failure_event(
                                    &source,
                                    signature_state.response_id.as_deref(),
                                    USAGE_NOT_RECORDED_MESSAGE,
                                );
                                ended = true;
                            }
                        }
                    }
                }
                let event_type = event["type"].as_str().unwrap_or("response.failed");
                let sse_bytes = format!("event: {event_type}\ndata: {event}\n\n");
                let signature = signature_state.record_event(&event, sse_bytes.as_bytes());

                if let Some((response_id, response_hash)) = signature {
                    persist_response_attestation(
                        attestation_service.as_ref(),
                        owner,
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
            description = "Polymorphic success response. For non-OpenAI models, the typed adapter returns ResponseObject metadata with hosted search events/citations, client-managed function/custom call items, and optional namespaces as JSON for non-streaming requests. Canonical openai/ models forward the provider-native response shape, including native reasoning, web_search_call items, and search citations or sources returned by the provider. Streaming requests use text/event-stream on either path. Inference-Id is one UUID for POST /v1/billing/costs. Typed non-streaming responses with multiple rounds also expose their ordered usage IDs in Inference-Ids; streaming responses carry the first round's ID. A response whose usage cannot be confirmed is returned as a server error or ends with response.failed.",
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
            model = %model,
            "Processing streaming response request"
        );

        // Create streaming response
        match service
            .create_response_stream_with_billing(
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
            Ok((stream, billing)) => {
                tracing::debug!("Successfully created streaming response");

                // Headers carry the identifier known when the stream starts:
                // the first inference round's. Later hosted-search rounds are
                // billed under their own identifiers.
                let mut stream = stream.fuse();
                let (prelude, inference_id) = peek_first_inference_id(&mut stream, &billing).await;
                let stream: ResponseEventStream =
                    Box::pin(futures::stream::iter(prelude).chain(stream));
                let byte_stream = signed_response_sse_stream(
                    stream,
                    client_tool_adapter,
                    state.attestation_service.clone(),
                    api_key.signature_owner(),
                    body_hash.hash.clone(),
                    Some(UsageConfirmation {
                        usage_service: state.native_service.usage_service.clone(),
                        organization_id: api_key.organization.id.0,
                        billing,
                        metrics_service: state.metrics_service.clone(),
                    }),
                );

                // Return as raw byte stream with SSE headers
                let mut response = Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-store")
                    .header(header::CONNECTION, "keep-alive")
                    .body(Body::from_stream(byte_stream))
                    .unwrap();
                if let Some(inference_id) = inference_id {
                    insert_inference_id_header(response.headers_mut(), &[inference_id]);
                }
                response
            }
            Err(error) => {
                tracing::error!(
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
            model = %model,
            "Processing non-streaming response request"
        );

        // Service only supports streaming - collect stream for non-streaming response
        match service
            .create_response_stream_with_billing(
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
            Ok((stream, billing)) => {
                tracing::debug!(
                    "Successfully created stream, collecting events for non-streaming response"
                );

                let response = match collect_terminal_response(stream).await {
                    Ok(response) => response,
                    Err(response) => return *response,
                };

                // Confirm usage for every billed inference round, then attempt
                // the existing gateway signature write before returning. The
                // signature stores only the response ID and digests, never the
                // response body; its failures leave the response unchanged.
                billed_non_streaming_response(
                    response,
                    &client_tool_adapter,
                    state.attestation_service.as_ref(),
                    api_key.signature_owner(),
                    body_hash.hash.clone(),
                    &UsageConfirmation {
                        usage_service: state.native_service.usage_service.clone(),
                        organization_id: api_key.organization.id.0,
                        billing,
                        metrics_service: state.metrics_service.clone(),
                    },
                )
                .await
            }
            Err(error) => {
                tracing::error!(
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
    use services::metrics::capturing::{CapturingMetricsService, MetricValue};
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

    fn test_owner() -> SignatureOwner {
        SignatureOwner {
            organization_id: uuid::Uuid::from_u128(1),
            workspace_id: uuid::Uuid::from_u128(2),
        }
    }

    #[derive(Clone, Default)]
    struct RecordingAttestationService {
        stored_response_signatures: Arc<Mutex<Vec<(String, String, String)>>>,
        owners: Arc<Mutex<Vec<SignatureOwner>>>,
    }

    impl RecordingAttestationService {
        fn stored_response_signatures(&self) -> Vec<(String, String, String)> {
            self.stored_response_signatures
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        fn owners(&self) -> Vec<SignatureOwner> {
            self.owners
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    #[async_trait]
    impl AttestationServiceTrait for RecordingAttestationService {
        async fn get_chat_signature(
            &self,
            _owner: SignatureOwner,
            _chat_id: &str,
            _signing_algo: Option<String>,
        ) -> Result<SignatureLookupResult, AttestationError> {
            Err(AttestationError::InternalError("unused".to_string()))
        }

        async fn store_chat_signature_from_provider(
            &self,
            _owner: SignatureOwner,
            _chat_id: &str,
        ) -> Result<(), AttestationError> {
            Ok(())
        }

        async fn store_chat_signature(
            &self,
            _owner: SignatureOwner,
            _chat_id: &str,
            _request_hash: String,
            _response_hash: String,
        ) -> Result<(), AttestationError> {
            Ok(())
        }

        async fn store_response_signature(
            &self,
            owner: SignatureOwner,
            response_id: &str,
            request_hash: String,
            response_hash: String,
        ) -> Result<(), AttestationError> {
            self.owners
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(owner);
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
            test_owner(),
            "request-hash".to_string(),
            None,
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
            test_owner(),
            "request-digest".to_string(),
            None,
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
        assert_eq!(attestation.owners(), vec![test_owner()]);
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
            test_owner(),
            "request-digest".to_string(),
            None,
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
            test_owner(),
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
            test_owner(),
            "custom-request-digest".to_string(),
            None,
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
            test_owner(),
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

    /// Serves `/v1/billing/costs` lookups from rows that become visible after a
    /// number of lookups, as the completion layer's asynchronous writes do.
    #[derive(Default)]
    struct CostLookupUsageService {
        rows: Mutex<Vec<Uuid>>,
        visible_after_lookups: usize,
        lookups: Mutex<Vec<(Uuid, Vec<Uuid>)>>,
    }

    impl CostLookupUsageService {
        fn with_rows(rows: Vec<Uuid>, visible_after_lookups: usize) -> Arc<Self> {
            Arc::new(Self {
                rows: Mutex::new(rows),
                visible_after_lookups,
                lookups: Mutex::default(),
            })
        }

        fn lookups(&self) -> Vec<(Uuid, Vec<Uuid>)> {
            self.lookups
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    #[async_trait]
    impl UsageServiceTrait for CostLookupUsageService {
        async fn record_usage(
            &self,
            _request: services::usage::RecordUsageServiceRequest,
        ) -> Result<services::usage::UsageLogEntry, services::usage::UsageError> {
            unimplemented!()
        }

        async fn record_usage_from_api(
            &self,
            _organization_id: Uuid,
            _workspace_id: Uuid,
            _api_key_id: Uuid,
            _request: services::usage::RecordUsageApiRequest,
            _discount: Option<services::usage::UsageDiscount>,
        ) -> Result<services::usage::UsageLogEntry, services::usage::UsageError> {
            unimplemented!()
        }

        async fn check_can_use(
            &self,
            _organization_id: Uuid,
        ) -> Result<services::usage::UsageCheckResult, services::usage::UsageError> {
            unimplemented!()
        }

        async fn get_balance(
            &self,
            _organization_id: Uuid,
        ) -> Result<Option<services::usage::OrganizationBalanceInfo>, services::usage::UsageError>
        {
            unimplemented!()
        }

        async fn get_usage_history(
            &self,
            _organization_id: Uuid,
            _limit: Option<i64>,
            _offset: Option<i64>,
        ) -> Result<(Vec<services::usage::UsageLogEntry>, i64), services::usage::UsageError>
        {
            unimplemented!()
        }

        async fn get_limit(
            &self,
            _organization_id: Uuid,
        ) -> Result<Option<services::usage::OrganizationLimit>, services::usage::UsageError>
        {
            unimplemented!()
        }

        async fn get_credit_limits(
            &self,
            _organization_id: Uuid,
        ) -> Result<Vec<services::usage::OrganizationCreditLimit>, services::usage::UsageError>
        {
            unimplemented!()
        }

        async fn get_usage_history_by_api_key(
            &self,
            _api_key_id: Uuid,
            _credit_type: Option<&str>,
            _limit: Option<i64>,
            _offset: Option<i64>,
        ) -> Result<(Vec<services::usage::UsageLogEntry>, i64), services::usage::UsageError>
        {
            unimplemented!()
        }

        async fn get_api_key_usage_history_with_permissions(
            &self,
            _workspace_id: Uuid,
            _api_key_id: Uuid,
            _user_id: Uuid,
            _credit_type: Option<&str>,
            _limit: Option<i64>,
            _offset: Option<i64>,
        ) -> Result<(Vec<services::usage::UsageLogEntry>, i64), services::usage::UsageError>
        {
            unimplemented!()
        }

        async fn get_costs_by_inference_ids(
            &self,
            organization_id: Uuid,
            inference_ids: Vec<Uuid>,
        ) -> Result<Vec<services::usage::InferenceCost>, services::usage::UsageError> {
            let lookup_count = {
                let mut lookups = self
                    .lookups
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                lookups.push((organization_id, inference_ids.clone()));
                lookups.len()
            };
            if lookup_count <= self.visible_after_lookups {
                return Ok(vec![]);
            }
            let rows = self
                .rows
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            Ok(inference_ids
                .into_iter()
                .filter(|id| rows.contains(id))
                .map(|inference_id| services::usage::InferenceCost {
                    inference_id,
                    cost_nano_usd: 1_000,
                })
                .collect())
        }

        async fn get_usage_by_model(
            &self,
            _organization_id: Uuid,
            _start_date: chrono::DateTime<chrono::Utc>,
            _end_date: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<Vec<services::usage::UsageByModelEntry>, services::usage::UsageError> {
            unimplemented!()
        }

        async fn list_inference_usage_history(
            &self,
            _query: services::usage::InferenceUsageHistoryQuery,
        ) -> Result<(Vec<services::usage::InferenceUsageReportRow>, i64), services::usage::UsageError>
        {
            unimplemented!()
        }
    }

    fn inference_id(provider_id: &str) -> Uuid {
        services::completions::hash_inference_id_to_uuid(provider_id)
    }

    /// Billing for a hosted-search response: a search round, then an answer.
    fn two_round_billing() -> ResponseBilling {
        let billing = ResponseBilling::default();
        billing.begin_round();
        billing.record_provider_id("chatcmpl-search-round");
        billing.record_usage(30, 6);
        billing.begin_round();
        billing.record_provider_id("chatcmpl-answer-round");
        billing.record_usage(90, 20);
        billing
    }

    fn confirmation(
        usage_service: Arc<CostLookupUsageService>,
        organization_id: Uuid,
        billing: ResponseBilling,
    ) -> UsageConfirmation {
        confirmation_with_metrics(usage_service, organization_id, billing).0
    }

    fn confirmation_with_metrics(
        usage_service: Arc<CostLookupUsageService>,
        organization_id: Uuid,
        billing: ResponseBilling,
    ) -> (UsageConfirmation, Arc<CapturingMetricsService>) {
        let metrics = Arc::new(CapturingMetricsService::new());
        let confirmation = UsageConfirmation {
            usage_service,
            organization_id,
            billing,
            metrics_service: metrics.clone(),
        };
        (confirmation, metrics)
    }

    /// The tags of the confirmation counter and latency, which must match.
    fn confirmation_metric_tags(metrics: &CapturingMetricsService) -> Vec<String> {
        let recorded = metrics.get_metrics();
        assert_eq!(recorded.len(), 2, "one counter and one latency");
        let counter = recorded
            .iter()
            .find(|metric| metric.name == METRIC_RESPONSES_USAGE_CONFIRMATION)
            .expect("confirmation counter");
        assert!(matches!(counter.value, MetricValue::Count(1)));
        let latency = recorded
            .iter()
            .find(|metric| metric.name == METRIC_RESPONSES_USAGE_CONFIRMATION_DURATION)
            .expect("confirmation latency");
        assert!(matches!(latency.value, MetricValue::Latency(_)));
        assert_eq!(counter.tags, latency.tags);
        let mut tags = counter.tags.clone();
        tags.sort();
        tags
    }

    fn environment_tag() -> String {
        format!("{TAG_ENVIRONMENT}:{}", get_environment())
    }

    #[tokio::test(start_paused = true)]
    async fn non_streaming_response_waits_for_every_round_row_and_exposes_its_ids() {
        let ids = vec![
            inference_id("chatcmpl-search-round"),
            inference_id("chatcmpl-answer-round"),
        ];
        // The rows become visible on the third lookup, after two retries.
        let usage = CostLookupUsageService::with_rows(ids.clone(), 2);
        let organization_id = Uuid::new_v4();
        let attestation = RecordingAttestationService::default();
        let mut response = sample_response("resp_billed");
        response.usage = Usage::new(120, 26);
        let (confirmation, metrics) =
            confirmation_with_metrics(usage.clone(), organization_id, two_round_billing());

        let http_response = billed_non_streaming_response(
            response,
            &ClientToolAdapter::default(),
            &attestation,
            test_owner(),
            "request-digest".to_string(),
            &confirmation,
        )
        .await;

        assert_eq!(http_response.status(), StatusCode::OK);
        let mut expected = vec![environment_tag(), format!("{TAG_RESULT}:confirmed")];
        expected.sort();
        assert_eq!(confirmation_metric_tags(&metrics), expected);
        assert_eq!(
            http_response.headers()[HEADER_INFERENCE_ID],
            ids[0].to_string().as_str()
        );
        assert_eq!(
            http_response.headers()[HEADER_INFERENCE_IDS],
            format!("{}, {}", ids[0], ids[1]).as_str()
        );
        assert_eq!(
            http_response.headers()[header::ACCESS_CONTROL_EXPOSE_HEADERS],
            "Inference-Id, Inference-Ids"
        );
        let lookups = usage.lookups();
        assert_eq!(lookups.len(), 3);
        assert!(lookups
            .iter()
            .all(|(org, requested)| *org == organization_id && *requested == ids));
        assert_eq!(attestation.stored_response_signatures().len(), 1);
        let body = axum::body::to_bytes(http_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["usage"]["total_tokens"], 146);
    }

    #[tokio::test(start_paused = true)]
    async fn non_streaming_response_without_a_usage_row_is_not_a_success() {
        // Only the answer round was recorded; the search round's row is missing.
        let usage =
            CostLookupUsageService::with_rows(vec![inference_id("chatcmpl-answer-round")], 0);
        let attestation = RecordingAttestationService::default();
        let (confirmation, metrics) =
            confirmation_with_metrics(usage.clone(), Uuid::new_v4(), two_round_billing());

        let started = tokio::time::Instant::now();
        let http_response = billed_non_streaming_response(
            sample_response("resp_unbilled"),
            &ClientToolAdapter::default(),
            &attestation,
            test_owner(),
            "request-digest".to_string(),
            &confirmation,
        )
        .await;

        assert_eq!(http_response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let mut expected = vec![
            environment_tag(),
            format!("{TAG_RESULT}:failed"),
            format!("{TAG_REASON}:missing_rows"),
        ];
        expected.sort();
        assert_eq!(confirmation_metric_tags(&metrics), expected);
        assert!(http_response.headers().get(HEADER_INFERENCE_ID).is_none());
        assert!(started.elapsed() <= RESPONSE_USAGE_CONFIRM_TIMEOUT);
        assert!(usage.lookups().len() > 1, "the lookup must be retried");
        assert!(attestation.stored_response_signatures().is_empty());
        let body = axum::body::to_bytes(http_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.error.message, USAGE_NOT_RECORDED_MESSAGE);
        assert_eq!(error.error.r#type, "server_error");
    }

    #[tokio::test]
    async fn round_without_final_usage_fails_before_any_lookup() {
        let billing = ResponseBilling::default();
        billing.begin_round();
        billing.record_provider_id("chatcmpl-no-usage");
        let usage = CostLookupUsageService::with_rows(vec![inference_id("chatcmpl-no-usage")], 0);
        let attestation = RecordingAttestationService::default();
        let (confirmation, metrics) =
            confirmation_with_metrics(usage.clone(), Uuid::new_v4(), billing);

        let http_response = billed_non_streaming_response(
            sample_response("resp_no_usage"),
            &ClientToolAdapter::default(),
            &attestation,
            test_owner(),
            "request-digest".to_string(),
            &confirmation,
        )
        .await;

        assert_eq!(http_response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let mut expected = vec![
            environment_tag(),
            format!("{TAG_RESULT}:failed"),
            format!("{TAG_REASON}:unreported_usage"),
        ];
        expected.sort();
        assert_eq!(confirmation_metric_tags(&metrics), expected);
        assert!(usage.lookups().is_empty());
        assert!(attestation.stored_response_signatures().is_empty());
    }

    #[tokio::test]
    async fn response_without_billable_rounds_is_not_a_success() {
        let usage = CostLookupUsageService::with_rows(vec![], 0);
        let http_response = billed_non_streaming_response(
            sample_response("resp_empty"),
            &ClientToolAdapter::default(),
            &RecordingAttestationService::default(),
            test_owner(),
            "request-digest".to_string(),
            &confirmation(usage.clone(), Uuid::new_v4(), ResponseBilling::default()),
        )
        .await;
        assert_eq!(http_response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(http_response.headers().get(HEADER_INFERENCE_ID).is_none());
        assert!(usage.lookups().is_empty());
    }

    #[tokio::test]
    async fn streaming_completed_frame_is_released_after_usage_is_confirmed() {
        let ids = vec![
            inference_id("chatcmpl-search-round"),
            inference_id("chatcmpl-answer-round"),
        ];
        let usage = CostLookupUsageService::with_rows(ids.clone(), 0);
        let attestation = Arc::new(RecordingAttestationService::default());
        let mut stream = signed_response_sse_stream(
            Box::pin(futures::stream::iter([
                stream_event("response.created", Some(sample_response("resp_stream"))),
                stream_event("response.completed", Some(sample_response("resp_stream"))),
            ])),
            ClientToolAdapter::default(),
            attestation.clone(),
            test_owner(),
            "request-digest".to_string(),
            Some(confirmation(
                usage.clone(),
                Uuid::new_v4(),
                two_round_billing(),
            )),
        );

        stream.next().await.unwrap().unwrap();
        assert!(usage.lookups().is_empty(), "only completion is confirmed");
        let completed = frame_json(&stream.next().await.unwrap().unwrap());
        assert_eq!(completed["type"], "response.completed");
        assert_eq!(completed["inference_ids"], serde_json::json!(ids));
        assert_eq!(usage.lookups().len(), 1);
        assert_eq!(attestation.stored_response_signatures().len(), 1);
        assert!(stream.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn streaming_completion_without_usage_rows_becomes_a_failed_frame() {
        let usage = CostLookupUsageService::with_rows(vec![], 0);
        let attestation = Arc::new(RecordingAttestationService::default());
        let mut completed = sample_response("resp_stream_unbilled");
        completed.usage = Usage::new(120, 26);
        let mut completed = stream_event("response.completed", Some(completed));
        completed.sequence_number = Some(7);
        let mut stream = signed_response_sse_stream(
            Box::pin(futures::stream::iter([
                stream_event(
                    "response.created",
                    Some(sample_response("resp_stream_unbilled")),
                ),
                completed,
            ])),
            ClientToolAdapter::default(),
            attestation.clone(),
            test_owner(),
            "request-digest".to_string(),
            Some(confirmation(
                usage.clone(),
                Uuid::new_v4(),
                two_round_billing(),
            )),
        );

        stream.next().await.unwrap().unwrap();
        let frame = stream.next().await.unwrap().unwrap();
        assert!(std::str::from_utf8(&frame)
            .unwrap()
            .starts_with("event: response.failed\n"));
        let failed = frame_json(&frame);
        assert_eq!(failed["sequence_number"], 7);
        assert_eq!(failed["response"]["id"], "resp_stream_unbilled");
        assert_eq!(failed["response"]["status"], "failed");
        assert_eq!(
            failed["response"]["error"]["message"],
            USAGE_NOT_RECORDED_MESSAGE
        );
        assert_eq!(failed["response"]["usage"]["total_tokens"], 146);
        assert_eq!(failed["response"]["output"], serde_json::json!([]));
        assert_typed_failure_event(&failed, 7);
        assert!(stream.next().await.is_none());
        assert!(attestation.stored_response_signatures().is_empty());
    }

    /// The emitted frame must parse as the typed stream event that clients
    /// use, with a complete failed response and the replaced sequence number.
    fn assert_typed_failure_event(event: &serde_json::Value, sequence: u64) {
        let typed: ResponseStreamEvent = serde_json::from_value(event.clone())
            .expect("response.failed must match ResponseStreamEvent");
        assert_eq!(typed.event_type, "response.failed");
        assert_eq!(typed.sequence_number, Some(sequence));
        let response = typed.response.expect("response.failed carries a response");
        assert_eq!(response.status, ResponseStatus::Failed);
        assert!(response.output.is_empty());
        assert!(response.tools.is_empty());
        assert!(response.instructions.is_none());
        assert!(!response.store);
    }

    #[tokio::test]
    async fn stream_peek_holds_events_until_the_first_round_identity() {
        let billing = ResponseBilling::default();
        let worker_billing = billing.clone();
        let mut delta = stream_event("response.output_text.delta", None);
        delta.delta = Some("Hi".to_string());
        // The service records the round's provider ID before emitting any
        // event derived from its first chunk.
        let mut stream = futures::stream::iter(vec![
            stream_event("response.created", Some(sample_response("resp_peek"))),
            stream_event("response.in_progress", Some(sample_response("resp_peek"))),
            delta,
            stream_event("response.completed", Some(sample_response("resp_peek"))),
        ])
        .map(move |event| {
            if event.event_type == "response.output_text.delta" {
                worker_billing.begin_round();
                worker_billing.record_provider_id("chatcmpl-peek");
            }
            event
        })
        .fuse();

        let (prelude, first_id) = peek_first_inference_id(&mut stream, &billing).await;

        assert_eq!(first_id, Some(inference_id("chatcmpl-peek")));
        let held: Vec<_> = prelude
            .iter()
            .map(|event| event.event_type.as_str())
            .collect();
        assert_eq!(
            held,
            [
                "response.created",
                "response.in_progress",
                "response.output_text.delta"
            ]
        );
        let rest: Vec<_> = stream.collect().await;
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].event_type, "response.completed");
    }

    #[tokio::test]
    async fn stream_peek_stops_at_a_terminal_event_without_an_identity() {
        let billing = ResponseBilling::default();
        let mut stream = futures::stream::iter(vec![
            stream_event("response.created", Some(sample_response("resp_failed"))),
            stream_event("response.failed", Some(sample_response("resp_failed"))),
            stream_event("response.completed", Some(sample_response("resp_failed"))),
        ])
        .fuse();
        let (prelude, first_id) = peek_first_inference_id(&mut stream, &billing).await;
        assert_eq!(first_id, None);
        assert_eq!(prelude.len(), 2);
        assert_eq!(prelude[1].event_type, "response.failed");
    }

    #[tokio::test]
    async fn non_streaming_collection_never_turns_partial_output_into_success() {
        let mut delta = stream_event("response.output_text.delta", None);
        delta.delta = Some("partial".to_string());
        let unterminated: ResponseEventStream = Box::pin(futures::stream::iter(vec![
            stream_event("response.created", Some(sample_response("resp_partial"))),
            delta,
        ]));
        let Err(response) = collect_terminal_response(unterminated).await else {
            panic!("an unterminated stream is not a response");
        };
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let mut failed = stream_event("response.failed", None);
        failed.status_code = Some(502);
        let failed: ResponseEventStream = Box::pin(futures::stream::iter(vec![failed]));
        let Err(response) = collect_terminal_response(failed).await else {
            panic!("a failed stream is not a response");
        };
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        let completed: ResponseEventStream = Box::pin(futures::stream::iter(vec![stream_event(
            "response.completed",
            Some(sample_response("resp_done")),
        )]));
        let Ok(response) = collect_terminal_response(completed).await else {
            panic!("a completed stream is a response");
        };
        assert_eq!(response.id, "resp_done");
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
            test_owner(),
            "request-digest".to_string(),
            None,
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
        assert_typed_failure_event(&event, 2);
        assert!(!event.to_string().contains("malformed wrapper"));
        assert!(stream.next().await.is_none());
        assert!(attestation.stored_response_signatures().is_empty());

        let http_response = signed_non_streaming_response(
            response,
            &request.client_tool_adapter,
            attestation.as_ref(),
            test_owner(),
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
