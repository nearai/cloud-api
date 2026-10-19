use crate::{
    middleware::{auth::AuthenticatedApiKey, RequestBodyHash, RequestCorrelation, RequestedModel},
    models::*,
    routes::{
        api::AppState,
        common::{
            alias_warning_message, inject_chat_warning_field, inject_warning_field,
            map_domain_error_to_status, no_aliasing_requested, HEADER_MODEL_ALIAS_RESOLVED,
            HEADER_NO_ALIASING,
        },
        extractors::OpenAiJson,
        files::MAX_FILE_SIZE,
    },
};
use axum::{
    body::{Body, Bytes},
    extract::{Extension, Multipart, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json as ResponseJson, Response},
};
use futures::stream::StreamExt;
use services::auto_redact::{self, AutoRedactError, RedactionMap, StreamUnredact};
use services::common::encryption_headers as service_encryption_headers;
use services::completions::{
    hash_inference_id_to_uuid,
    ports::{CompletionMessage, CompletionRequest as ServiceCompletionRequest},
    MAX_PROVIDER_DONE_EVENT_BYTES,
};
use sha2::{Digest, Sha256};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, Instrument};
use utoipa;
use uuid::Uuid;

mod chat;
mod chat_auto_redact;
mod chat_stream;
mod models;
mod text;

pub use chat::*;
use chat_auto_redact::*;
use chat_stream::*;
pub use models::*;
pub use text::*;

mod audio;
mod billing;
mod embeddings;
mod images;
mod privacy;
mod rerank;
mod score;

pub use audio::*;
pub use embeddings::*;
pub use images::*;
pub use privacy::*;
pub use rerank::*;
pub use score::*;

// Timeout for synchronous usage recording before response is returned
const USAGE_RECORDING_TIMEOUT_SECS: u64 = 5;

// Bound the final Gateway-signature write before exposing a synthesized SSE
// terminator. The provider routing pin has already been released on this path.
const STREAM_SIGNATURE_STORE_TIMEOUT_SECS: u64 = 5;

// Upper bound on leading SSE control events (keepalive comments, blank
// lines) consumed while peeking for the first parsed chunk. Real upstreams
// emit zero before the first data chunk; the cap stops a misbehaving or
// malicious upstream from stalling response start or growing the stash
// unbounded. Past the cap we proceed without an Inference-Id header and let
// the remaining stream (including the buffered control events) flow through.
const MAX_LEADING_CONTROL_EVENTS: usize = 32;

// A deferred upstream terminal contains only `data: [DONE]` and, when the
// lossless parser emits it separately, its blank SSE separator.
const MAX_HELD_PROVIDER_TERMINAL_BYTES: usize = MAX_PROVIDER_DONE_EVENT_BYTES;

/// Insert validated E2EE headers into a provider `extra` HashMap.
fn insert_encryption_headers(
    encryption_headers: &crate::routes::common::EncryptionHeaders,
    extra: &mut std::collections::HashMap<String, serde_json::Value>,
) {
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
    if let Some(ref encryption_version) = encryption_headers.encryption_version {
        extra.insert(
            service_encryption_headers::ENCRYPTION_VERSION.to_string(),
            serde_json::Value::String(encryption_version.clone()),
        );
    }
    if let Some(ref encrypt_all_fields) = encryption_headers.encrypt_all_fields {
        extra.insert(
            service_encryption_headers::ENCRYPT_ALL_FIELDS.to_string(),
            serde_json::Value::String(encrypt_all_fields.clone()),
        );
    }
}

fn insert_request_id_header(
    correlation: RequestCorrelation,
    extra: &mut std::collections::HashMap<String, serde_json::Value>,
) {
    extra.insert(
        "x_request_id".to_string(),
        serde_json::Value::String(correlation.request_id.to_string()),
    );
}

// Custom header for exposing the inference ID as a UUID. Shared with the
// native Anthropic Messages plane so /v1/billing/costs lookups work the same
// way against both.
pub(crate) const HEADER_INFERENCE_ID: &str = "Inference-Id";

// Custom header surfacing which trust tier served a completion.
// Value is "near" for NEAR AI's own TEE fleet, "non-attested" for external
// (non-TEE) providers, or the attested third party's source name ("chutes",
// "tinfoil").
const HEADER_SERVING_PROVIDER: &str = "x-serving-provider";

/// True when a client encryption header was supplied. `X-Model-Pub-Key` alone
/// only pins routing and does not make the request or response encrypted.
/// E2EE bodies are opaque to the gateway, so alias warnings use the response
/// header rather than being injected into the body.
fn e2ee_requested(encryption_headers: &crate::routes::common::EncryptionHeaders) -> bool {
    encryption_headers.signing_algo.is_some()
        || encryption_headers.client_pub_key.is_some()
        || encryption_headers.encryption_version.is_some()
        || encryption_headers.encrypt_all_fields.is_some()
}

/// Enforce the `x-no-aliasing` strict mode (issue #573): if the client set
/// the header and the requested model is an alias, refuse with 400 before
/// any inference happens — i.e. before tokens are billed and, for E2EE
/// clients, before a payload is bound to a different model TD's signing key.
///
/// Unknown models fall through (`Ok`) so the completion service returns its
/// canonical "not a valid model name or alias" error.
async fn reject_if_aliased(
    models_service: &Arc<dyn services::models::ModelsServiceTrait>,
    headers: &header::HeaderMap,
    requested_model: &str,
) -> Result<(), Response> {
    if !no_aliasing_requested(headers) {
        return Ok(());
    }
    match models_service.resolve_and_get_model(requested_model).await {
        Ok(m) if m.model_name != requested_model => Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                format!(
                    "Model '{requested_model}' is an alias of '{}' and the request set \
                     {HEADER_NO_ALIASING}. Use the canonical model name '{}'.",
                    m.model_name, m.model_name
                ),
                "model_alias_rejected".to_string(),
            )),
        )
            .into_response()),
        Ok(_) => Ok(()),
        Err(services::models::ModelsError::NotFound(_)) => Ok(()),
        Err(_) => {
            // Strict mode must fail closed: if the catalog can't be
            // consulted, we can't guarantee no alias was applied.
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to resolve model for x-no-aliasing check".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
                .into_response())
        }
    }
}

struct ImageUsageRecord<'a> {
    organization_id: Uuid,
    workspace_id: Uuid,
    api_key_id: Uuid,
    model_id: Uuid,
    provider_request_id: &'a str,
    image_count: i32,
    provider_attribution: services::usage::ProviderAttribution,
    inference_type: services::usage::InferenceType,
}

/// Build a `RecordUsageServiceRequest` for image operations (generation or editing).
fn build_image_usage_request(
    record: ImageUsageRecord<'_>,
) -> services::usage::RecordUsageServiceRequest {
    services::usage::RecordUsageServiceRequest {
        inference_id: Some(hash_inference_id_to_uuid(record.provider_request_id)),
        provider_request_id: Some(record.provider_request_id.to_string()),
        stop_reason: Some(services::usage::StopReason::Completed),
        image_count: Some(record.image_count),
        provider_attribution: record.provider_attribution,
        ..services::usage::RecordUsageServiceRequest::base(
            services::usage::BillingIdentity {
                organization_id: record.organization_id,
                workspace_id: record.workspace_id,
                api_key_id: record.api_key_id,
                model_id: record.model_id,
            },
            record.inference_type,
        )
    }
}

/// Record usage synchronously with timeout, falling back to async retry.
/// Used for non-streaming operations (image gen/edit) where usage should be
/// persisted before the HTTP response is returned.
pub(super) async fn record_usage_with_sync_fallback(
    usage_service: Arc<dyn services::usage::UsageServiceTrait + Send + Sync>,
    request: services::usage::RecordUsageServiceRequest,
    operation_label: &str,
) {
    let provider_request_id = request.provider_request_id.clone().unwrap_or_default();
    let organization_id = request.organization_id;
    let timeout_duration = Duration::from_secs(USAGE_RECORDING_TIMEOUT_SECS);

    match tokio::time::timeout(
        timeout_duration,
        usage_service.record_usage(request.clone()),
    )
    .await
    {
        Ok(Ok(_)) => {
            tracing::debug!(
                image_id = %provider_request_id,
                %organization_id,
                "{operation_label} usage recorded synchronously"
            );
        }
        Ok(Err(e)) => {
            tracing::warn!(
                error = %e,
                image_id = %provider_request_id,
                "Failed to record usage synchronously, retrying async"
            );
            spawn_async_usage_retry(usage_service, request, provider_request_id, operation_label);
        }
        Err(_timeout) => {
            tracing::warn!(
                image_id = %provider_request_id,
                "Usage recording timed out ({USAGE_RECORDING_TIMEOUT_SECS}s), retrying async"
            );
            spawn_async_usage_retry(usage_service, request, provider_request_id, operation_label);
        }
    }
}

/// Spawn an async retry for usage recording after a sync attempt fails or times out.
fn spawn_async_usage_retry(
    usage_service: Arc<dyn services::usage::UsageServiceTrait + Send + Sync>,
    request: services::usage::RecordUsageServiceRequest,
    provider_request_id: String,
    operation_label: &str,
) {
    let label = operation_label.to_string();
    tokio::spawn(async move {
        if let Err(e) = usage_service.record_usage(request).await {
            tracing::error!(
                error = %e,
                image_id = %provider_request_id,
                "Failed to record {label} usage in async retry"
            );
        }
    });
}

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

#[cfg(test)]
mod tests;
