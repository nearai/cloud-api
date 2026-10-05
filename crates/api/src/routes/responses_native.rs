//! HTTP adaptation for the native stateless Responses service.
use super::{common, completions::HEADER_INFERENCE_ID};
use crate::{middleware::auth::AuthenticatedApiKey, models::ErrorResponse};
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::Value;
use services::{
    completions::hash_inference_id_to_uuid,
    models::ModelWithPricing,
    responses::native::{NativeResponsesContext, NativeResponsesError, NativeResponsesService},
};
use uuid::Uuid;

pub(super) async fn handle(
    service: NativeResponsesService,
    model: ModelWithPricing,
    api_key: AuthenticatedApiKey,
    headers: HeaderMap,
    request_hash: String,
    body: Value,
) -> Response {
    let encryption = match common::validate_encryption_headers(&headers) {
        Ok(headers) => headers,
        Err(response) => return response.into_response(),
    };
    if encryption.signing_algo.is_some()
        || encryption.client_pub_key.is_some()
        || encryption.model_pub_key.is_some()
        || encryption.encryption_version.is_some()
        || encryption.encrypt_all_fields.is_some()
    {
        return error(
            StatusCode::BAD_REQUEST,
            "Encryption headers are unavailable for native OpenAI Responses",
        );
    }
    let requested = body["model"]
        .as_str()
        .expect("selected request has a model");
    if model.model_name != requested && common::no_aliasing_requested(&headers) {
        return error(
            StatusCode::BAD_REQUEST,
            "Model alias resolution is disabled for this request",
        );
    }
    let alias =
        (requested != model.model_name).then(|| format!("{requested} -> {}", model.model_name));
    let api_key_id = match Uuid::parse_str(&api_key.api_key.id.0) {
        Ok(id) => id,
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Invalid billing context"),
    };
    let context = NativeResponsesContext {
        request_hash,
        organization_id: api_key.organization.id.0,
        workspace_id: api_key.workspace.id.0,
        api_key_id,
        fallback_enabled: api_key.organization.fallback_enabled(),
        system_prompt: api_key.organization.settings["system_prompt"]
            .as_str()
            .map(str::to_owned),
    };
    match service.execute(model, body, context).await {
        Ok(result) => response(
            result.upstream.status,
            result.upstream.headers,
            Body::from_stream(result.upstream.body),
            alias,
            result.provider_response_id.as_deref(),
        ),
        Err(e) => match e {
            NativeResponsesError::InvalidRequest(message) => {
                error(StatusCode::BAD_REQUEST, message)
            }
            NativeResponsesError::RateLimited => error(
                StatusCode::TOO_MANY_REQUESTS,
                "Concurrent request limit exceeded",
            ),
            NativeResponsesError::Provider(message) => error(StatusCode::BAD_GATEWAY, message),
            NativeResponsesError::Internal(message) => {
                error(StatusCode::INTERNAL_SERVER_ERROR, message)
            }
        },
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(ErrorResponse::new(
            message.into(),
            if status.is_client_error() {
                "invalid_request_error"
            } else {
                "api_error"
            }
            .into(),
        )),
    )
        .into_response()
}

fn response(
    status: StatusCode,
    upstream_headers: HeaderMap,
    body: Body,
    alias: Option<String>,
    id: Option<&str>,
) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    for (name, value) in upstream_headers {
        if let Some(name) =
            name.filter(|n| matches!(n.as_str(), "content-type" | "x-request-id" | "retry-after"))
        {
            response.headers_mut().insert(name, value);
        }
    }
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        "x-serving-provider",
        HeaderValue::from_static("non-attested"),
    );
    if let Some(id) = id {
        response.headers_mut().insert(
            HEADER_INFERENCE_ID,
            HeaderValue::from_str(&hash_inference_id_to_uuid(id).to_string()).unwrap(),
        );
    }
    if let Some(alias) = alias.and_then(|s| HeaderValue::from_str(&s).ok()) {
        response
            .headers_mut()
            .insert(common::HEADER_MODEL_ALIAS_RESOLVED, alias);
    }
    response
}
