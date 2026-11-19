use super::{
    add_model_headers, signatures, valid_message_id, AnthropicRouteError, MessageSigningContext,
    RouteResult,
};
use crate::middleware::auth::AuthenticatedApiKey;
use crate::routes::{
    api::AppState,
    common::map_domain_error_to_status,
    completions::{convert_chat_request_to_service, session_hint_from_headers},
};
use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use inference_providers::{ChatCompletionResponse, ServingProvider, TokenUsage};
use serde_json::{json, Value};
use services::completions::{hash_inference_id_to_uuid, CompletionError};
use uuid::Uuid;

mod request;
mod stream;
#[cfg(test)]
mod tests;

fn invalid(message: impl Into<String>) -> AnthropicRouteError {
    AnthropicRouteError::new(StatusCode::BAD_REQUEST, "invalid_request_error", message)
}

fn upstream(message: &'static str) -> AnthropicRouteError {
    AnthropicRouteError::new(StatusCode::BAD_GATEWAY, "api_error", message)
}

fn service_error(error: CompletionError) -> AnthropicRouteError {
    let status = map_domain_error_to_status(&error);
    let kind = match status {
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        StatusCode::NOT_FOUND => "not_found_error",
        StatusCode::FORBIDDEN => "permission_error",
        StatusCode::UNAUTHORIZED => "authentication_error",
        status if status.is_client_error() => "invalid_request_error",
        _ => "api_error",
    };
    let message = if status.is_server_error() {
        "Chat completion failed".to_owned()
    } else {
        crate::models::ErrorResponse::from(error).error.message
    };
    AnthropicRouteError::new(status, kind, message)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn respond(
    state: &AppState,
    key: &AuthenticatedApiKey,
    body: Value,
    model: &services::models::ModelWithPricing,
    request_id: Uuid,
    signing: MessageSigningContext,
    headers: &HeaderMap,
    alias: Option<String>,
) -> RouteResult<Response> {
    let supports_effort = model.supported_features.iter().any(|f| f == "reasoning")
        || model
            .supported_sampling_parameters
            .iter()
            .any(|p| p == "reasoning_effort" || p == "reasoning");
    let model = model.model_name.as_str();
    let request = request::translate(&body, model, supports_effort)?;
    let streaming = request.stream == Some(true);
    let show_thinking = match body.pointer("/thinking/display").map(Value::as_str) {
        None | Some(Some("summarized")) => matches!(
            body.pointer("/thinking/type").and_then(Value::as_str),
            Some("enabled" | "adaptive")
        ),
        Some(Some("omitted")) => false,
        _ => return Err(invalid("Unsupported thinking display")),
    };
    drop(body);
    let mut completion = convert_chat_request_to_service(
        &request,
        key.api_key.created_by_user_id.0,
        key.api_key.id.0.clone(),
        key.organization.id.0,
        key.workspace.id.0,
        key.organization.fallback_enabled(),
        key.organization.request_priority,
        signing.request_hash.clone(),
        None,
        request_id,
    );
    drop(request);
    completion.skip_provider_chat_signature = true;
    completion.session_hint = session_hint_from_headers(headers);

    let (mut response, id, serving) = if streaming {
        let source = state
            .completion_service
            .create_chat_completion_stream(completion)
            .await
            .map_err(service_error)?;
        let (source, id) = stream::convert(source, model.to_owned(), show_thinking).await?;
        let serving = match id.as_deref() {
            Some(id) => state
                .inference_provider_pool
                .get_provider_by_chat_id(id)
                .await
                .map(|p| ServingProvider::of(p.as_ref())),
            None => None,
        };
        let mut response = Response::new(Body::from_stream(signatures::signed_stream(
            source, signing,
        )));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        (response, id, serving)
    } else {
        let result = state
            .completion_service
            .create_chat_completion(completion)
            .await
            .map_err(service_error)?;
        let message = from_chat(&result.response, model, show_thinking)?;
        let bytes = serde_json::to_vec(&message).map_err(|_| upstream("Invalid chat response"))?;
        let id = result.response.id;
        signing
            .store(id.clone(), signatures::body_hash(&bytes))
            .await;
        let mut response = Response::new(Body::from(bytes));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        (response, Some(id), Some(result.serving))
    };
    if let Some(serving) = serving {
        response.headers_mut().insert(
            "x-serving-provider",
            HeaderValue::from_static(serving.label()),
        );
    }
    add_model_headers(
        &mut response,
        alias,
        id.as_deref().map(hash_inference_id_to_uuid),
    );
    Ok(response)
}

fn usage(value: &TokenUsage) -> Value {
    let read = value.cached_tokens().max(0);
    let write = value.cache_write_tokens().max(0);
    json!({"input_tokens":value.prompt_tokens.max(0)-read-write, "output_tokens":value.completion_tokens.max(0),
        "cache_read_input_tokens":read, "cache_creation_input_tokens":write})
}

fn stop_reason(reason: Option<&str>) -> RouteResult<&'static str> {
    match reason {
        Some("stop") => Ok("end_turn"),
        Some("tool_calls" | "function_call") => Ok("tool_use"),
        Some("length") => Ok("max_tokens"),
        Some("content_filter") => Ok("refusal"),
        _ => Err(upstream(
            "Chat response is missing a supported finish reason",
        )),
    }
}

fn message(id: &str, model: &str, content: Value, reason: Option<&str>, usage: Value) -> Value {
    json!({"id":id,"type":"message","role":"assistant","model":model,"content":content,
        "stop_reason":reason,"stop_sequence":null,"usage":usage})
}

fn tool_input(raw: &str) -> RouteResult<Value> {
    let value: Value =
        serde_json::from_str(raw).map_err(|_| upstream("Invalid tool arguments from model"))?;
    if !value.is_object() {
        return Err(upstream("Tool arguments must be an object"));
    }
    Ok(value)
}

fn from_chat(
    chat: &ChatCompletionResponse,
    model: &str,
    show_thinking: bool,
) -> RouteResult<Value> {
    if !valid_message_id(&chat.id) {
        return Err(upstream("Chat response is missing a valid ID"));
    }
    let choice = chat
        .choices
        .iter()
        .find(|c| c.index == 0)
        .ok_or_else(|| upstream("Chat response has no completion"))?;
    let msg = &choice.message;
    let reason = stop_reason(choice.finish_reason.as_deref())?;
    let mut content = Vec::new();
    if show_thinking {
        if let Some(thought) = msg
            .reasoning_content
            .as_ref()
            .or(msg.reasoning.as_ref())
            .filter(|t| !t.is_empty())
        {
            content.push(json!({"type":"thinking","thinking":thought,"signature":""}));
        }
    }
    for text in [msg.content.as_deref(), msg.refusal.as_deref()]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
    {
        content.push(json!({"type":"text","text":text}));
    }
    for call in msg.tool_calls.iter().flatten() {
        let id = call
            .id
            .as_deref()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| upstream("Tool call has no ID"))?;
        let name = call
            .function
            .name
            .as_deref()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| upstream("Tool call has no name"))?;
        let input = match tool_input(call.function.arguments.as_deref().unwrap_or("")) {
            Err(_) if reason == "max_tokens" => continue,
            result => result?,
        };
        content.push(json!({"type":"tool_use","id":id,"name":name,"input":input}));
    }
    if reason == "tool_use" && !content.iter().any(|block| block["type"] == "tool_use") {
        return Err(upstream("Chat response ended with a missing tool call"));
    }
    Ok(message(
        &chat.id,
        model,
        content.into(),
        Some(reason),
        usage(&chat.usage),
    ))
}
