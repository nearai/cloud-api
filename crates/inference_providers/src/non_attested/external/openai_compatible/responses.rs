//! Responses transport for OpenAI models whose function tools require it.
//!
//! Keep the public chat contract and the provider/billing pipeline unchanged.
//! Requests are stateless: replay chat history as messages, function calls and
//! function outputs, without enabling upstream storage or inventing response IDs.
//! Chat history has no slot for encrypted Responses reasoning items. We replay
//! visible history with fresh function_call items (no upstream item IDs), rather
//! than referencing a stored response whose reasoning context is unavailable.

use super::{BackendConfig, OpenAiCompatibleBackend};
use crate::{
    chunk_builder::ChunkContext, BufferedSSEParser, ChatCompletionParams, ChatCompletionResponse,
    ChatCompletionResponseWithBytes, CompletionError, FinishReason, MessageRole, SSEEventParser,
    StreamChunk, StreamingResult, TokenUsage, ToolChoice,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;

fn invalid_request(message: impl Into<String>) -> CompletionError {
    CompletionError::HttpError {
        status_code: 400,
        message: message.into(),
        is_external: true,
    }
}

fn invalid_response(message: impl Into<String>) -> CompletionError {
    CompletionError::InvalidResponse(message.into())
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, CompletionError> {
    value[key]
        .as_str()
        .ok_or_else(|| invalid_response(format!("OpenAI Responses payload is missing {key}")))
}

pub(super) fn build_request(
    params: &ChatCompletionParams,
    model: &str,
    stream: bool,
) -> Result<Value, CompletionError> {
    if params.n.is_some_and(|n| n != 1) {
        return Err(invalid_request("GPT-6 Astra supports only n=1"));
    }
    if params.extra.contains_key("tools") || params.extra.contains_key("tool_choice") {
        return Err(invalid_request(
            "Unsupported tool definition or tool_choice for GPT-6 Astra",
        ));
    }
    if params
        .tools
        .iter()
        .flatten()
        .any(|tool| tool.type_ != "function")
    {
        return Err(invalid_request(
            "Only function tools are supported for GPT-6 Astra",
        ));
    }
    let mut input = Vec::new();
    for message in &params.messages {
        if message.role == MessageRole::Tool {
            let id = message
                .tool_call_id
                .as_deref()
                .ok_or_else(|| invalid_request("Tool messages require tool_call_id"))?;
            let output = match &message.content {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) | None => String::new(),
                Some(value) => value.to_string(),
            };
            input.push(json!({"type": "function_call_output", "call_id": id, "output": output}));
            continue;
        }
        if let Some(content) = &message.content {
            if !content.is_null() {
                let content = convert_content(content, message.role == MessageRole::Assistant)?;
                input.push(json!({"role": message.role, "content": content}));
            }
        }
        for call in message.tool_calls.iter().flatten() {
            let id = call
                .id
                .as_deref()
                .ok_or_else(|| invalid_request("Tool calls require an id"))?;
            let name = call
                .function
                .name
                .as_deref()
                .ok_or_else(|| invalid_request("Tool calls require a function name"))?;
            input.push(json!({
                "type": "function_call", "call_id": id, "name": name,
                "arguments": call.function.arguments.as_deref().unwrap_or("")
            }));
        }
    }
    let mut body = json!({"model": model, "input": input, "stream": stream, "store": false});
    // The completion service leaves several chat fields in extra instead of
    // populating their typed slots. Prefer typed values, then their passthrough
    // equivalents, before falling back to the legacy max_tokens field.
    if let Some(limit) = params
        .max_completion_tokens
        .or_else(|| {
            params
                .extra
                .get("max_completion_tokens")
                .and_then(Value::as_i64)
        })
        .or(params.max_tokens)
    {
        body["max_output_tokens"] = json!(limit);
    }
    if let Some(effort) = params.extra.get("reasoning_effort") {
        // Astra cannot disable reasoning. Keep explicitly requested reasoning
        // levels; map older clients' disabled/minimal modes to its lowest level.
        let effort = match effort.as_str() {
            Some("none" | "minimal") => json!("low"),
            _ => effort.clone(),
        };
        body["reasoning"] = json!({"effort": effort});
    }
    if let Some(tools) = &params.tools {
        body["tools"] = Value::Array(
            tools
                .iter()
                .map(|tool| {
                    // Responses defaults to strict schemas, while Chat Completions
                    // defaults to non-strict. Preserve an explicit strict selector from
                    // the original request, which the typed FunctionDefinition omits.
                    let strict = params
                        .original_request
                        .as_ref()
                        .and_then(|r| r["tools"].as_array())
                        .and_then(|tools| {
                            tools
                                .iter()
                                .find(|t| t["function"]["name"] == tool.function.name)
                        })
                        .and_then(|t| t["function"]["strict"].as_bool())
                        .unwrap_or(false);
                    let mut result = json!({
                        "type": "function", "name": tool.function.name,
                        "parameters": tool.function.parameters, "strict": strict
                    });
                    if let Some(description) = &tool.function.description {
                        result["description"] = json!(description);
                    }
                    result
                })
                .collect(),
        );
    }
    if let Some(choice) = &params.tool_choice {
        body["tool_choice"] = match choice {
            ToolChoice::String(s) => json!(s),
            ToolChoice::Function { function, .. } => {
                json!({"type": "function", "name": function.name})
            }
        };
    }
    if let Some(parallel) = params.parallel_tool_calls.or_else(|| {
        params
            .extra
            .get("parallel_tool_calls")
            .and_then(Value::as_bool)
    }) {
        body["parallel_tool_calls"] = json!(parallel);
    }
    if let Some(tier) = params.service_tier {
        body["service_tier"] = json!(tier);
    }
    if let Some(user) = &params.user {
        body["user"] = json!(user);
    }
    // Only forward fields with the same meaning in both protocols. Chat-only
    // sampling/logprob fields (including temperature/top_p) are unsupported by
    // Astra. Do not forward arbitrary flattened chat fields to Responses.
    for key in [
        "metadata",
        "user",
        "safety_identifier",
        "prompt_cache_key",
        "prompt_cache_options",
    ] {
        if let Some(value) = params.extra.get(key) {
            body[key] = value.clone();
        }
    }
    if let Some(metadata) = &params.metadata {
        body["metadata"] = metadata.clone();
    }
    if let Some(format) = params.extra.get("response_format") {
        body["text"]["format"] = if format["type"] == "json_schema" {
            let mut schema = format["json_schema"].clone();
            if !schema.is_object() {
                return Err(invalid_request(
                    "response_format.json_schema must be an object",
                ));
            }
            schema["type"] = json!("json_schema");
            // Preserve Chat's non-strict default here too.
            if schema.get("strict").is_none() {
                schema["strict"] = json!(false);
            }
            schema
        } else {
            format.clone()
        };
    }
    if let Some(verbosity) = params.extra.get("verbosity") {
        body["text"]["verbosity"] = verbosity.clone();
    }
    Ok(body)
}

fn convert_content(content: &Value, assistant: bool) -> Result<Value, CompletionError> {
    if content.is_string() {
        return Ok(content.clone());
    }
    let parts = content
        .as_array()
        .ok_or_else(|| invalid_request("Message content must be a string or array"))?;
    let mut result = Vec::new();
    for part in parts {
        result.push(match part["type"].as_str() {
            Some("text") => {
                let text = part["text"]
                    .as_str()
                    .ok_or_else(|| invalid_request("Text parts require text"))?;
                if assistant {
                    json!({"type": "output_text", "text": text, "annotations": []})
                } else {
                    json!({"type": "input_text", "text": text})
                }
            }
            Some("image_url") if !assistant => {
                let image = &part["image_url"];
                let url = image["url"]
                    .as_str()
                    .or_else(|| image.as_str())
                    .ok_or_else(|| invalid_request("Image parts require image_url.url"))?;
                let mut image_part = json!({"type": "input_image", "image_url": url});
                if let Some(detail) = image.get("detail") {
                    image_part["detail"] = detail.clone();
                }
                image_part
            }
            _ => {
                return Err(invalid_request(
                    "Unsupported message content part for GPT-6 Astra",
                ))
            }
        });
    }
    Ok(json!(result))
}

fn usage(response: &Value) -> Result<TokenUsage, CompletionError> {
    let usage = &response["usage"];
    let count = |key: &str| {
        usage[key]
            .as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .filter(|n| *n >= 0)
            .ok_or_else(|| {
                invalid_response(format!(
                    "OpenAI Responses usage is missing or invalid: {key}"
                ))
            })
    };
    Ok(TokenUsage {
        prompt_tokens: count("input_tokens")?,
        // output_tokens includes reasoning tokens; never bill only visible text.
        completion_tokens: count("output_tokens")?,
        total_tokens: count("total_tokens")?,
        prompt_tokens_details: usage
            .get("input_tokens_details")
            .filter(|v| !v.is_null())
            .cloned(),
    })
}

fn finish_reason(response: &Value, has_tools: bool) -> Result<FinishReason, CompletionError> {
    match response["status"].as_str() {
        Some("completed") => Ok(if has_tools {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        }),
        Some("incomplete") => match response["incomplete_details"]["reason"].as_str() {
            Some("max_output_tokens") => Ok(FinishReason::Length),
            Some("content_filter") => Ok(FinishReason::ContentFilter),
            _ => Err(invalid_response(
                "Unknown OpenAI Responses incomplete reason",
            )),
        },
        Some("failed") => Err(provider_error(&response["error"])),
        _ => Err(invalid_response(
            "OpenAI Responses returned a non-terminal response",
        )),
    }
}

fn provider_error(error: &Value) -> CompletionError {
    CompletionError::HttpError {
        status_code: if error["code"] == "rate_limit_exceeded" {
            429
        } else {
            502
        },
        message: error["message"]
            .as_str()
            .unwrap_or("OpenAI Responses request failed")
            .to_string(),
        is_external: true,
    }
}

fn convert_response(
    response: &Value,
    model: &str,
) -> Result<ChatCompletionResponse, CompletionError> {
    if response["status"] == "failed" {
        return Err(provider_error(&response["error"]));
    }
    let output = response["output"]
        .as_array()
        .ok_or_else(|| invalid_response("OpenAI Responses output is missing"))?;
    let mut text = String::new();
    let mut refusal = String::new();
    let mut calls = Vec::new();
    for item in output {
        match item["type"].as_str() {
            Some("message") => {
                for part in item["content"].as_array().into_iter().flatten() {
                    match part["type"].as_str() {
                        Some("output_text") => text.push_str(string(part, "text")?),
                        Some("refusal") => refusal.push_str(string(part, "refusal")?),
                        _ => {}
                    }
                }
            }
            Some("function_call") => calls.push(json!({
                "id": string(item, "call_id")?, "type": "function",
                "function": {"name": string(item, "name")?, "arguments": string(item, "arguments")?}
            })),
            _ => {} // Reasoning items are not assistant text.
        }
    }
    let reason = finish_reason(response, !calls.is_empty())?;
    serde_json::from_value(json!({
        "id": string(response, "id")?, "object": "chat.completion",
        "created": response["created_at"], "model": model,
        "service_tier": response["service_tier"], "usage": usage(response)?,
        "choices": [{"index": 0, "finish_reason": reason, "message": {
            "role": "assistant",
            "content": if text.is_empty() { Value::Null } else { json!(text) },
            "refusal": if refusal.is_empty() { Value::Null } else { json!(refusal) },
            "tool_calls": if calls.is_empty() { Value::Null } else { json!(calls) }
        }}]
    }))
    .map_err(|e| invalid_response(format!("Invalid OpenAI Responses completion: {e}")))
}

pub(super) async fn send(
    backend: &OpenAiCompatibleBackend,
    config: &BackendConfig,
    model: &str,
    params: &ChatCompletionParams,
    stream: bool,
) -> Result<reqwest::Response, CompletionError> {
    let body = build_request(params, model, stream)?;
    let response = backend
        .client
        .post(format!(
            "{}/responses",
            config.base_url.trim_end_matches('/')
        ))
        .headers(
            backend
                .build_headers(config)
                .map_err(CompletionError::CompletionError)?,
        )
        .timeout(std::time::Duration::from_secs(
            config.timeout_seconds as u64,
        ))
        .json(&body)
        .send()
        .await
        .map_err(|e| CompletionError::CompletionError(e.to_string()))?;
    if !response.status().is_success() {
        let status_code = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        return Err(CompletionError::HttpError {
            status_code,
            message: crate::extract_error_message(&body),
            is_external: true,
        });
    }
    Ok(response)
}

pub(super) async fn completion(
    backend: &OpenAiCompatibleBackend,
    config: &BackendConfig,
    model: &str,
    params: &ChatCompletionParams,
) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
    let response = send(backend, config, model, params, false)
        .await?
        .json::<Value>()
        .await
        .map_err(|e| invalid_response(e.to_string()))?;
    let response = convert_response(&response, model)?;
    let raw_bytes = serde_json::to_vec(&response).map_err(|e| invalid_response(e.to_string()))?;
    Ok(ChatCompletionResponseWithBytes {
        response,
        raw_bytes,
        serving_tier: crate::ProviderTier::NonAttested,
    })
}

struct ParserState {
    context: ChunkContext,
    // Responses output_index also counts reasoning and messages. Chat tool
    // indices must be dense and stable even with interleaved function deltas.
    tools: HashMap<u64, i64>,
    service_tier: Option<String>,
    finished: bool,
}

struct EventParser;
impl SSEEventParser for EventParser {
    type State = ParserState;

    fn parse_event(
        state: &mut ParserState,
        data: &str,
    ) -> Result<Option<StreamChunk>, CompletionError> {
        if data == "[DONE]" || state.finished {
            return Ok(None);
        }
        let event: Value = serde_json::from_str(data)
            .map_err(|e| invalid_response(format!("Invalid OpenAI Responses event: {e}")))?;
        let mut chunk = match event["type"].as_str() {
            Some("response.created") => {
                let response = &event["response"];
                state.context.id = string(response, "id")?.to_string();
                state.context.created = response["created_at"]
                    .as_i64()
                    .ok_or_else(|| invalid_response("Missing response.created_at"))?;
                state.service_tier = response["service_tier"].as_str().map(str::to_string);
                state.context.role_chunk()
            }
            Some("response.output_text.delta") => state
                .context
                .text_chunk(string(&event, "delta")?.to_string()),
            Some("response.refusal.delta") => {
                let mut chunk = state.context.text_chunk(String::new());
                let delta = chunk.choices[0].delta.as_mut().unwrap();
                delta.content = None;
                delta
                    .extra
                    .insert("refusal".to_string(), json!(string(&event, "delta")?));
                chunk
            }
            Some("response.output_item.added") if event["item"]["type"] == "function_call" => {
                let output_index = event["output_index"]
                    .as_u64()
                    .ok_or_else(|| invalid_response("Missing tool output_index"))?;
                let index = state.tools.len() as i64;
                if state.tools.insert(output_index, index).is_some() {
                    return Err(invalid_response("Duplicate OpenAI Responses tool index"));
                }
                let mut chunk = state.context.tool_call_start_chunk(
                    index,
                    string(&event["item"], "call_id")?.to_string(),
                    string(&event["item"], "name")?.to_string(),
                );
                chunk.choices[0]
                    .delta
                    .as_mut()
                    .unwrap()
                    .tool_calls
                    .as_mut()
                    .unwrap()[0]
                    .function
                    .as_mut()
                    .unwrap()
                    .arguments = Some(
                    event["item"]["arguments"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                );
                chunk
            }
            Some("response.function_call_arguments.delta") => {
                let index = event["output_index"]
                    .as_u64()
                    .and_then(|i| state.tools.get(&i))
                    .ok_or_else(|| {
                        invalid_response("Arguments received for an unknown tool call")
                    })?;
                state
                    .context
                    .tool_call_args_chunk(*index, string(&event, "delta")?.to_string())
            }
            Some("response.completed" | "response.incomplete") => {
                let response = &event["response"];
                let reason = finish_reason(response, !state.tools.is_empty())?;
                let usage = usage(response)?;
                if let Some(tier) = response["service_tier"].as_str() {
                    state.service_tier = Some(tier.to_string());
                }
                state.finished = true;
                state.context.finish_chunk(Some(reason), usage)
            }
            Some("response.failed") => return Err(provider_error(&event["response"]["error"])),
            Some("error") => return Err(provider_error(&event)),
            _ => return Ok(None), // Done events repeat text/arguments; never emit them twice.
        };
        chunk.service_tier = state.service_tier.clone();
        Ok(Some(StreamChunk::Chat(chunk)))
    }
}

pub(super) fn parse_stream<S>(stream: S, model: String) -> StreamingResult
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin + Send + 'static,
{
    let state = ParserState {
        context: ChunkContext::new(String::new(), model, 0),
        tools: HashMap::new(),
        service_tier: None,
        finished: false,
    };
    let mut parser = BufferedSSEParser::<_, EventParser>::new(stream, state);
    Box::pin(async_stream::try_stream! {
        while let Some(event) = parser.next().await {
            let event = event?;
            let finished = matches!(&event.chunk, Some(StreamChunk::Chat(chunk)) if chunk.choices.iter().any(|c| c.finish_reason.is_some()));
            yield event;
            if finished { return; }
        }
        Err(invalid_response("OpenAI Responses stream ended before a terminal event"))?;
    })
}

#[cfg(test)]
mod tests;
