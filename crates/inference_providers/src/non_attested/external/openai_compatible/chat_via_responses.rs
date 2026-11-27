//! Chat Completions over OpenAI's Responses API for GPT-6 Astra.
//!
//! OpenAI's `/v1/chat/completions` rejects function tools while a model
//! reasons, and GPT-6 Astra models do not accept `reasoning_effort: "none"`,
//! so every Astra chat request that carries `tools` would fail there. Those
//! requests are sent to `/v1/responses` instead (stateless, `store: false`),
//! and the response, or the SSE stream, is translated back to chat format.

use super::{is_openai_source, BackendConfig, OpenAiCompatibleBackend, CATALOG_MODEL_NAME_KEY};
use crate::chunk_builder::ChunkContext;
use crate::sse_parser::{BufferedSSEParser, SSEEventParser};
use crate::{
    ChatCompletionParams, ChatCompletionResponse, ChatCompletionResponseChoice,
    ChatCompletionResponseWithBytes, ChatResponseMessage, CompletionError, CompletionTokensDetails,
    FinishReason, FunctionCall, MessageRole, StreamChunk, StreamingResult, TokenUsage, ToolCall,
    ToolChoice,
};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

use super::super::backend::ExternalBackend;

/// Cap on an upstream error body we read into memory.
const MAX_ERROR_BODY_BYTES: usize = 1024 * 1024;
/// Cap on a non-streaming upstream response body.
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// Chat-only sampling knobs the Responses API rejects. Dropped from `extra`.
const DROPPED_EXTRA_KEYS: &[&str] = &[
    "frequency_penalty",
    "presence_penalty",
    "seed",
    "logit_bias",
    "top_k",
    "repetition_penalty",
    "min_p",
    "top_a",
    "stream_options",
    "n",
    "stop",
    "max_tokens",
    "max_completion_tokens",
    crate::CACHE_SALT_FIELD,
];

/// Keys of `extra` that are translated or validated rather than forwarded.
const TRANSLATED_EXTRA_KEYS: &[&str] = &[
    "reasoning_effort",
    "reasoning",
    "verbosity",
    "response_format",
    "logprobs",
    "top_logprobs",
    "modalities",
];

/// True for `gpt-6-astra` and `gpt-6.<digits>-astra`, each optionally followed
/// by `-<suffix>` (a dated snapshot).
fn is_gpt_6_astra(name: &str) -> bool {
    let name = name.rsplit('/').next().unwrap_or(name);
    let Some(rest) = name.strip_prefix("gpt-6") else {
        return false;
    };
    let rest = match rest.strip_prefix('.') {
        Some(after_dot) => {
            let trimmed = after_dot.trim_start_matches(|c: char| c.is_ascii_digit());
            if trimmed.len() == after_dot.len() {
                return false;
            }
            trimmed
        }
        None => rest,
    };
    match rest.strip_prefix("-astra") {
        Some(tail) => tail.is_empty() || tail.starts_with('-'),
        None => false,
    }
}

/// Whether a chat request must go through `/v1/responses`.
pub(super) fn routes_via_responses(
    params: &ChatCompletionParams,
    base_url: &str,
    model: &str,
    catalog_model: Option<&str>,
) -> bool {
    params.tools.as_ref().is_some_and(|t| !t.is_empty())
        && is_openai_source(base_url)
        && (is_gpt_6_astra(model) || catalog_model.is_some_and(is_gpt_6_astra))
}

fn bad_request(message: &str) -> CompletionError {
    CompletionError::HttpError {
        status_code: 400,
        message: message.to_string(),
        is_external: false,
    }
}

fn upstream_failure(message: Option<&str>) -> CompletionError {
    CompletionError::HttpError {
        status_code: 502,
        message: message
            .filter(|m| !m.is_empty())
            .unwrap_or("Upstream response failed")
            .to_string(),
        is_external: true,
    }
}

// ---------------------------------------------------------------------------
// Request translation
// ---------------------------------------------------------------------------

fn convert_part(part: &Value) -> Result<Value, CompletionError> {
    const UNSUPPORTED: &str = "unsupported content part type for this model";
    let obj = part.as_object().ok_or_else(|| bad_request(UNSUPPORTED))?;
    match obj.get("type").and_then(Value::as_str) {
        Some("text") => {
            let text = obj
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| bad_request("invalid text content part"))?;
            Ok(json!({"type": "input_text", "text": text}))
        }
        Some("image_url") => {
            let (url, nested_detail) = match obj.get("image_url") {
                Some(Value::String(url)) => (url.as_str(), None),
                Some(Value::Object(image)) => (
                    image
                        .get("url")
                        .and_then(Value::as_str)
                        .ok_or_else(|| bad_request("invalid image content part"))?,
                    image.get("detail").and_then(Value::as_str),
                ),
                _ => return Err(bad_request("invalid image content part")),
            };
            // The API layer keeps `detail` beside `image_url`; raw OpenAI
            // requests nest it inside.
            let detail = obj.get("detail").and_then(Value::as_str).or(nested_detail);
            Ok(json!({
                "type": "input_image",
                "image_url": url,
                "detail": detail.unwrap_or("auto"),
            }))
        }
        Some("file") => {
            // The API layer sends `{"type":"file","file_id":..}`; raw OpenAI
            // requests nest the fields in a `file` object.
            let file = match obj.get("file") {
                Some(Value::Object(file)) => file,
                None => obj,
                Some(_) => return Err(bad_request("invalid file content part")),
            };
            let mut out = Map::new();
            out.insert("type".to_string(), json!("input_file"));
            for key in ["file_data", "file_id", "filename"] {
                if let Some(value) = file.get(key).filter(|v| !v.is_null()) {
                    out.insert(key.to_string(), value.clone());
                }
            }
            if out.len() == 1 {
                return Err(bad_request("invalid file content part"));
            }
            Ok(Value::Object(out))
        }
        _ => Err(bad_request(UNSUPPORTED)),
    }
}

/// Convert chat content (string or parts array) to Responses input content.
fn convert_content(content: Option<&Value>) -> Result<Value, CompletionError> {
    match content {
        None | Some(Value::Null) => Ok(Value::String(String::new())),
        Some(Value::String(text)) => Ok(Value::String(text.clone())),
        Some(Value::Array(parts)) => Ok(Value::Array(
            parts
                .iter()
                .map(convert_part)
                .collect::<Result<Vec<_>, _>>()?,
        )),
        Some(_) => Err(bad_request("invalid message content")),
    }
}

/// Assistant text as a plain string; non-text parts are rejected.
fn assistant_text(content: Option<&Value>) -> Result<String, CompletionError> {
    match content {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for part in parts {
                if part.get("type").and_then(Value::as_str) != Some("text") {
                    return Err(bad_request(
                        "unsupported assistant content part type for this model",
                    ));
                }
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    out.push_str(text);
                }
            }
            Ok(out)
        }
        Some(_) => Err(bad_request("invalid message content")),
    }
}

fn convert_messages(params: &ChatCompletionParams) -> Result<Vec<Value>, CompletionError> {
    let mut input = Vec::with_capacity(params.messages.len());
    for message in &params.messages {
        match message.role {
            MessageRole::System | MessageRole::User => {
                let role = if message.role == MessageRole::System {
                    "system"
                } else {
                    "user"
                };
                input.push(json!({
                    "role": role,
                    "content": convert_content(message.content.as_ref())?,
                }));
            }
            MessageRole::Assistant => {
                let text = assistant_text(message.content.as_ref())?;
                if !text.is_empty() {
                    input.push(json!({"role": "assistant", "content": text}));
                }
                for call in message.tool_calls.iter().flatten() {
                    let call_id = call
                        .id
                        .as_deref()
                        .ok_or_else(|| bad_request("tool call is missing an id"))?;
                    let name = call
                        .function
                        .name
                        .as_deref()
                        .ok_or_else(|| bad_request("tool call is missing a function name"))?;
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call_id,
                        "name": name,
                        "arguments": call.function.arguments.as_deref().unwrap_or("{}"),
                    }));
                }
            }
            MessageRole::Tool => {
                let call_id = message
                    .tool_call_id
                    .as_deref()
                    .ok_or_else(|| bad_request("tool message is missing tool_call_id"))?;
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": convert_content(message.content.as_ref())?,
                }));
            }
        }
    }
    Ok(input)
}

fn convert_tools(params: &ChatCompletionParams) -> Result<Vec<Value>, CompletionError> {
    let mut tools = Vec::new();
    for tool in params.tools.iter().flatten() {
        if tool.type_ != "function" {
            return Err(bad_request("unsupported tool type for this model"));
        }
        // Responses defaults `strict` to true and Chat Completions to false;
        // keep the chat meaning unless the client asked for strict explicitly.
        let strict = tool.function.strict.unwrap_or(false);
        let mut out = Map::new();
        out.insert("type".to_string(), json!("function"));
        out.insert("name".to_string(), json!(tool.function.name));
        if let Some(description) = &tool.function.description {
            out.insert("description".to_string(), json!(description));
        }
        out.insert("parameters".to_string(), tool.function.parameters.clone());
        out.insert("strict".to_string(), json!(strict));
        tools.push(Value::Object(out));
    }
    Ok(tools)
}

fn convert_response_format(format: &Value) -> Result<Value, CompletionError> {
    const INVALID: &str = "unsupported response_format for this model";
    match format.get("type").and_then(Value::as_str) {
        Some("text") => Ok(json!({"type": "text"})),
        Some("json_object") => Ok(json!({"type": "json_object"})),
        Some("json_schema") => {
            let schema = format
                .get("json_schema")
                .and_then(Value::as_object)
                .ok_or_else(|| bad_request(INVALID))?;
            let name = schema
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| bad_request(INVALID))?;
            let body = schema.get("schema").ok_or_else(|| bad_request(INVALID))?;
            let mut out = Map::new();
            out.insert("type".to_string(), json!("json_schema"));
            out.insert("name".to_string(), json!(name));
            out.insert("schema".to_string(), body.clone());
            for key in ["strict", "description"] {
                if let Some(value) = schema.get(key).filter(|v| !v.is_null()) {
                    out.insert(key.to_string(), value.clone());
                }
            }
            Ok(Value::Object(out))
        }
        _ => Err(bad_request(INVALID)),
    }
}

/// Apply `f` to a top-level object field of the request body, creating (or
/// replacing a non-object value with) an empty object first.
fn with_object_field(
    body: &mut Map<String, Value>,
    key: &str,
    f: impl FnOnce(&mut Map<String, Value>),
) {
    let slot = body
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    if let Value::Object(map) = slot {
        f(map);
    }
}

/// Translate chat params to a stateless Responses request body (without `model`).
fn build_request(params: &ChatCompletionParams, stream: bool) -> Result<Value, CompletionError> {
    if !matches!(params.n, None | Some(1)) {
        return Err(bad_request(
            "n other than 1 is not supported for this model",
        ));
    }
    if params.stop.as_ref().is_some_and(|s| !s.is_empty()) {
        return Err(bad_request("stop is not supported for this model"));
    }
    // The chat route leaves these in `extra`; internal callers use the typed
    // fields. Check both.
    let extra = |key: &str| params.extra.get(key).filter(|v| !v.is_null());
    if params.logprobs == Some(true)
        || params.top_logprobs.is_some()
        || extra("logprobs").is_some_and(|v| v != &Value::Bool(false))
        || extra("top_logprobs").is_some()
    {
        return Err(bad_request("logprobs is not supported for this model"));
    }
    let text_only = |modalities: &[Value]| modalities == [json!("text")];
    if params
        .modalities
        .as_ref()
        .is_some_and(|m| m.len() != 1 || m.first().map(String::as_str) != Some("text"))
        || extra("modalities").is_some_and(|v| !v.as_array().is_some_and(|m| text_only(m)))
    {
        return Err(bad_request("modalities is not supported for this model"));
    }

    let mut body = Map::new();

    // Forward unknown extras verbatim; the typed fields below overwrite any
    // collision so a client cannot smuggle in `store`, `input`, etc.
    let mut dropped = 0usize;
    for (key, value) in &params.extra {
        if DROPPED_EXTRA_KEYS.contains(&key.as_str()) {
            dropped += 1;
        } else if !TRANSLATED_EXTRA_KEYS.contains(&key.as_str()) {
            body.insert(key.clone(), value.clone());
        }
    }
    if dropped > 0 {
        tracing::debug!(dropped, "Dropped chat-only params from Responses request");
    }

    body.insert("input".to_string(), Value::Array(convert_messages(params)?));
    body.insert("tools".to_string(), Value::Array(convert_tools(params)?));
    body.insert("store".to_string(), json!(false));
    body.insert("stream".to_string(), json!(stream));

    match &params.tool_choice {
        Some(ToolChoice::String(choice)) => {
            body.insert("tool_choice".to_string(), json!(choice));
        }
        Some(ToolChoice::Function { function, .. }) => {
            body.insert(
                "tool_choice".to_string(),
                json!({"type": "function", "name": function.name}),
            );
        }
        None => {}
    }
    if let Some(parallel) = params.parallel_tool_calls {
        body.insert("parallel_tool_calls".to_string(), json!(parallel));
    }
    if let Some(temperature) = params.temperature {
        body.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = params.top_p {
        body.insert("top_p".to_string(), json!(top_p));
    }
    if let Some(max) = params.max_completion_tokens.or(params.max_tokens) {
        body.insert("max_output_tokens".to_string(), json!(max));
    }
    if let Some(tier) = &params.service_tier {
        let tier = serde_json::to_value(tier).map_err(|_| bad_request("invalid service_tier"))?;
        body.insert("service_tier".to_string(), tier);
    }

    // reasoning: `reasoning_effort` fills `reasoning.effort` unless the
    // `reasoning` object already has one.
    if let Some(reasoning) = params.extra.get("reasoning") {
        let Value::Object(reasoning) = reasoning else {
            return Err(bad_request("invalid reasoning"));
        };
        body.insert("reasoning".to_string(), Value::Object(reasoning.clone()));
    }
    if let Some(effort) = params.extra.get("reasoning_effort") {
        let Value::String(effort) = effort else {
            return Err(bad_request("invalid reasoning_effort"));
        };
        with_object_field(&mut body, "reasoning", |r| {
            r.entry("effort".to_string())
                .or_insert_with(|| json!(effort));
        });
    }
    if let Some(verbosity) = params.extra.get("verbosity") {
        let Value::String(verbosity) = verbosity else {
            return Err(bad_request("invalid verbosity"));
        };
        with_object_field(&mut body, "text", |t| {
            t.insert("verbosity".to_string(), json!(verbosity));
        });
    }
    if let Some(format) = params.extra.get("response_format") {
        let format = convert_response_format(format)?;
        with_object_field(&mut body, "text", |t| {
            t.insert("format".to_string(), format);
        });
    }

    let body = Value::Object(body);
    // Forwarded extras must not turn this into a stateful request
    // (`previous_response_id`, `conversation`, `background`).
    if !crate::responses_raw::is_stateless(&body) {
        return Err(bad_request(
            "stateful Responses parameters are not supported on chat completions",
        ));
    }
    Ok(body)
}

// ---------------------------------------------------------------------------
// Response translation
// ---------------------------------------------------------------------------

fn count(usage: &Value, key: &str) -> Result<Option<i32>, CompletionError> {
    match usage.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|n| i32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| CompletionError::InvalidResponse("invalid usage".to_string())),
    }
}

/// Convert Responses `usage` to chat usage. Invalid counts are an error:
/// billing depends on them.
fn convert_usage(usage: &Value) -> Result<TokenUsage, CompletionError> {
    let missing = || CompletionError::InvalidResponse("missing usage".to_string());
    let prompt = count(usage, "input_tokens")?.ok_or_else(missing)?;
    let completion = count(usage, "output_tokens")?.ok_or_else(missing)?;
    let total = match count(usage, "total_tokens")? {
        Some(total) => total,
        None => prompt.checked_add(completion).ok_or_else(missing)?,
    };
    let cached = match usage.get("input_tokens_details") {
        Some(details) => count(details, "cached_tokens")?,
        None => None,
    };
    let reasoning = match usage.get("output_tokens_details") {
        Some(details) => count(details, "reasoning_tokens")?,
        None => None,
    };
    Ok(TokenUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
        prompt_tokens_details: cached.map(|c| json!({"cached_tokens": c})),
        completion_tokens_details: reasoning.map(|r| CompletionTokensDetails {
            reasoning_tokens: Some(r),
            ..Default::default()
        }),
        reasoning_tokens: None,
    })
}

fn error_message(resp: &Value) -> Option<&str> {
    resp.get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
}

/// Finish reason for a terminal response object.
/// Truncation wins over tool calls: a call cut off by the output limit has
/// incomplete arguments.
fn finish_reason(resp: &Value, saw_tool_call: bool) -> FinishReason {
    if resp.get("status").and_then(Value::as_str) == Some("incomplete") {
        match resp
            .get("incomplete_details")
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
        {
            Some("max_output_tokens") => return FinishReason::Length,
            Some("content_filter") => return FinishReason::ContentFilter,
            _ => {}
        }
    }
    if saw_tool_call {
        FinishReason::ToolCalls
    } else {
        FinishReason::Stop
    }
}

fn finish_reason_str(reason: &FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::Length => "length",
        FinishReason::ContentFilter => "content_filter",
        FinishReason::ToolCalls => "tool_calls",
    }
}

fn invalid(message: &str) -> CompletionError {
    CompletionError::InvalidResponse(message.to_string())
}

fn convert_response(resp: &Value) -> Result<ChatCompletionResponse, CompletionError> {
    if matches!(
        resp.get("status").and_then(Value::as_str),
        Some("failed" | "cancelled")
    ) {
        return Err(upstream_failure(error_message(resp)));
    }
    let id = resp
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("missing response id"))?;

    let mut text = String::new();
    let mut refusal = String::new();
    let mut tool_calls = Vec::new();
    for item in resp
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    match part.get("type").and_then(Value::as_str) {
                        Some("output_text") => {
                            if let Some(t) = part.get("text").and_then(Value::as_str) {
                                text.push_str(t);
                            }
                        }
                        Some("refusal") => {
                            if let Some(r) = part.get("refusal").and_then(Value::as_str) {
                                refusal.push_str(r);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("function_call") => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("function call is missing call_id"))?;
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("function call is missing name"))?;
                tool_calls.push(ToolCall {
                    id: Some(call_id.to_string()),
                    type_: Some("function".to_string()),
                    function: FunctionCall {
                        name: Some(name.to_string()),
                        arguments: Some(
                            item.get("arguments")
                                .and_then(Value::as_str)
                                .unwrap_or("{}")
                                .to_string(),
                        ),
                    },
                    index: None,
                    thought_signature: None,
                });
            }
            _ => {}
        }
    }

    let usage = convert_usage(
        resp.get("usage")
            .filter(|u| u.is_object())
            .ok_or_else(|| invalid("missing usage"))?,
    )?;
    let reason = finish_reason(resp, !tool_calls.is_empty());

    let message = ChatResponseMessage {
        role: MessageRole::Assistant,
        content: (!text.is_empty()).then_some(text),
        refusal: (!refusal.is_empty()).then_some(refusal),
        annotations: None,
        audio: None,
        function_call: None,
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        reasoning_content: None,
        reasoning: None,
    };
    Ok(ChatCompletionResponse {
        id: id.to_string(),
        object: "chat.completion".to_string(),
        created: resp.get("created_at").and_then(Value::as_i64).unwrap_or(0),
        model: resp
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        choices: vec![ChatCompletionResponseChoice {
            index: 0,
            message,
            logprobs: None,
            finish_reason: Some(finish_reason_str(&reason).to_string()),
            token_ids: None,
            extra: Default::default(),
        }],
        service_tier: resp
            .get("service_tier")
            .and_then(Value::as_str)
            .map(str::to_string),
        system_fingerprint: None,
        usage,
        prompt_logprobs: None,
        prompt_token_ids: None,
        kv_transfer_params: None,
        extra: Default::default(),
    })
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

/// Per-stream translation state.
#[derive(Default)]
pub(super) struct ResponsesChatState {
    ctx: Option<ChunkContext>,
    /// Responses `output_index` -> chat tool-call index.
    tool_indexes: HashMap<u64, i64>,
    saw_tool_call: bool,
}

pub(super) struct ResponsesChatEventParser;

impl ResponsesChatEventParser {
    fn context(state: &ResponsesChatState) -> Result<&ChunkContext, CompletionError> {
        state
            .ctx
            .as_ref()
            .ok_or_else(|| invalid("event before response.created"))
    }
}

impl SSEEventParser for ResponsesChatEventParser {
    type State = ResponsesChatState;

    fn parse_event(
        state: &mut Self::State,
        data: &str,
    ) -> Result<Option<StreamChunk>, CompletionError> {
        if data.trim() == "[DONE]" {
            return Ok(None);
        }
        let event: Value =
            serde_json::from_str(data).map_err(|_| invalid("Failed to parse event"))?;
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            return Ok(None);
        };
        let chunk = match kind {
            "response.created" => {
                let resp = event.get("response").unwrap_or(&Value::Null);
                let ctx = ChunkContext::new(
                    resp.get("id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("missing response id"))?
                        .to_string(),
                    resp.get("model")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    resp.get("created_at").and_then(Value::as_i64).unwrap_or(0),
                );
                let chunk = ctx.role_chunk();
                state.ctx = Some(ctx);
                chunk
            }
            "response.output_text.delta" => {
                let ctx = Self::context(state)?;
                match event.get("delta").and_then(Value::as_str) {
                    Some(delta) if !delta.is_empty() => ctx.text_chunk(delta.to_string()),
                    _ => return Ok(None),
                }
            }
            "response.refusal.delta" => {
                let ctx = Self::context(state)?;
                let Some(delta) = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .filter(|d| !d.is_empty())
                else {
                    return Ok(None);
                };
                let mut chunk = ctx.text_chunk(String::new());
                if let Some(d) = chunk.choices.first_mut().and_then(|c| c.delta.as_mut()) {
                    d.content = None;
                    d.extra.insert("refusal".to_string(), json!(delta));
                }
                chunk
            }
            "response.output_item.added" => {
                Self::context(state)?;
                let item = event.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) != Some("function_call") {
                    return Ok(None);
                }
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| invalid("missing output_index"))?;
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("function call is missing call_id"))?;
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("function call is missing name"))?;
                let next = i64::try_from(state.tool_indexes.len())
                    .map_err(|_| invalid("too many tool calls"))?;
                let index = *state.tool_indexes.entry(output_index).or_insert(next);
                state.saw_tool_call = true;
                let ctx = Self::context(state)?;
                ctx.tool_call_start_chunk(index, call_id.to_string(), name.to_string())
            }
            "response.function_call_arguments.delta" => {
                let ctx = Self::context(state)?;
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| invalid("missing output_index"))?;
                let index = *state
                    .tool_indexes
                    .get(&output_index)
                    .ok_or_else(|| invalid("arguments for unknown function call"))?;
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("missing arguments delta"))?;
                ctx.tool_call_args_chunk(index, delta.to_string())
            }
            "response.completed" | "response.incomplete" => {
                let ctx = Self::context(state)?;
                let resp = event.get("response").unwrap_or(&Value::Null);
                let usage = convert_usage(
                    resp.get("usage")
                        .filter(|u| u.is_object())
                        .ok_or_else(|| invalid("missing usage"))?,
                )?;
                let reason = finish_reason(resp, state.saw_tool_call);
                ctx.finish_chunk(Some(reason), usage)
            }
            "response.failed" => {
                let resp = event.get("response").unwrap_or(&Value::Null);
                return Err(upstream_failure(error_message(resp)));
            }
            "error" => {
                return Err(upstream_failure(
                    event.get("message").and_then(Value::as_str),
                ));
            }
            _ => return Ok(None),
        };
        Ok(Some(StreamChunk::Chat(chunk)))
    }
}

type ResponsesChatSseParser<S> = BufferedSSEParser<S, ResponsesChatEventParser, CompletionError>;

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// Read up to `cap` bytes of a body. `None` when the cap is exceeded.
async fn collect_capped(
    mut body: crate::responses_raw::ResponsesRawBody,
    cap: usize,
) -> Result<Option<Vec<u8>>, CompletionError> {
    let mut out = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        if out.len().saturating_add(chunk.len()) > cap {
            return Ok(None);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(Some(out))
}

/// Error for a non-success upstream status. The body is read with a cap and
/// never logged.
async fn http_error(response: crate::responses_raw::ResponsesRawResponse) -> CompletionError {
    let status_code = response.status.as_u16();
    let mut body = response.body;
    let mut bytes = Vec::new();
    while let Some(Ok(chunk)) = body.next().await {
        let room = MAX_ERROR_BODY_BYTES.saturating_sub(bytes.len());
        bytes.extend(chunk.iter().take(room));
        if bytes.len() >= MAX_ERROR_BODY_BYTES {
            break;
        }
    }
    CompletionError::HttpError {
        status_code,
        message: crate::extract_error_message(&String::from_utf8_lossy(&bytes)),
        is_external: true,
    }
}

pub(super) async fn chat_completion(
    backend: &OpenAiCompatibleBackend,
    config: &BackendConfig,
    model: &str,
    params: ChatCompletionParams,
) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
    let body = build_request(&params, false)?;
    let response = backend.responses_raw(config, model, body).await?;
    if !response.status.is_success() {
        return Err(http_error(response).await);
    }
    let bytes = collect_capped(response.body, MAX_RESPONSE_BYTES)
        .await?
        .ok_or_else(|| invalid("response too large"))?;
    let parsed: Value = serde_json::from_slice(&bytes)
        .map_err(|_| CompletionError::CompletionError("Failed to parse response".to_string()))?;
    let response = convert_response(&parsed)?;
    let raw_bytes = serde_json::to_vec(&response).map_err(|_| {
        CompletionError::CompletionError("Failed to serialize response".to_string())
    })?;
    Ok(ChatCompletionResponseWithBytes {
        response,
        raw_bytes,
        serving: crate::ServingProvider::EXTERNAL,
    })
}

pub(super) async fn chat_completion_stream(
    backend: &OpenAiCompatibleBackend,
    config: &BackendConfig,
    model: &str,
    params: ChatCompletionParams,
) -> Result<StreamingResult, CompletionError> {
    let body = build_request(&params, true)?;
    let response = backend.responses_raw(config, model, body).await?;
    if !response.status.is_success() {
        return Err(http_error(response).await);
    }
    // The raw bytes are Responses events, not chat SSE, so the parser keeps
    // `passthrough_raw()` off and the route layer re-serializes the chunks.
    let parser: ResponsesChatSseParser<_> =
        BufferedSSEParser::new(response.body, ResponsesChatState::default());
    Ok(Box::pin(parser))
}

/// Whether `config` carries a catalog model name that makes this request route
/// through Responses. Used by the call sites in the parent module.
pub(super) fn routes(params: &ChatCompletionParams, config: &BackendConfig, model: &str) -> bool {
    routes_via_responses(
        params,
        &config.base_url,
        model,
        config.extra.get(CATALOG_MODEL_NAME_KEY).map(String::as_str),
    )
}

#[cfg(test)]
#[path = "chat_via_responses_tests.rs"]
mod tests;
