//! Pure wire-format compatibility between OpenAI Chat Completions requests and
//! Anthropic Messages requests.
//!
//! This crate deliberately has no HTTP client, async runtime, Cloud API domain
//! types, or logging. The adapter consumes the original JSON value so fields do
//! not disappear in intermediate typed models. Every top-level parameter is
//! either mapped, rejected, or returned as a warning; nothing is silently
//! ignored.

use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

mod response;
mod stream;

pub use response::{convert_anthropic_response, ResponseOptions};
pub use stream::StreamState;

const DEFAULT_MAX_TOKENS: i64 = 4096;
const MAX_CACHE_BREAKPOINTS: usize = 4;
const MODELS_REJECTING_SAMPLING: &[&str] = &["claude-opus-4-7"];

/// Options supplied by the routing layer after model/alias resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertOptions {
    pub model: String,
    pub stream: bool,
}

/// A parameter which was intentionally not sent to Anthropic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionWarning {
    pub parameter: String,
    pub reason: &'static str,
}

/// Converted Anthropic request and its explicit compatibility warnings.
#[derive(Debug, Clone, PartialEq)]
pub struct ConvertedRequest {
    pub body: Value,
    pub warnings: Vec<ConversionWarning>,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("{message}")]
pub struct CompatError {
    pub parameter: Option<String>,
    pub message: String,
}

impl CompatError {
    fn at(parameter: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            parameter: Some(parameter.into()),
            message: message.into(),
        }
    }

    fn request(message: impl Into<String>) -> Self {
        Self {
            parameter: None,
            message: message.into(),
        }
    }
}

/// Convert an original OpenAI Chat Completions JSON value to an Anthropic
/// Messages JSON value.
///
/// The output uses one canonical representation: every message and system
/// prompt uses content-block arrays, regardless of whether prompt caching is
/// enabled. Moving a `cache_control` marker therefore never changes the bytes
/// of an earlier block except for the marker itself.
pub fn convert_openai_request(
    input: &Value,
    options: &ConvertOptions,
) -> Result<ConvertedRequest, CompatError> {
    let request = input
        .as_object()
        .ok_or_else(|| CompatError::request("request body must be a JSON object"))?;

    reject_premium_features(request)?;

    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| CompatError::at("messages", "messages must be an array"))?;
    if messages.is_empty() {
        return Err(CompatError::at(
            "messages",
            "messages must contain at least one message",
        ));
    }

    let mut warnings = Vec::new();
    let (mut system, mut messages) = convert_messages(messages, &mut warnings)?;
    let mut tools = convert_tools(request.get("tools"), &mut warnings)?;
    clamp_cache_breakpoints(
        &mut tools,
        &mut system,
        &mut messages,
        request
            .get("cache_control")
            .is_some_and(|value| !value.is_null()),
    );

    let max_tokens = integer_parameter(request, "max_completion_tokens")?
        .or(integer_parameter(request, "max_tokens")?)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    if max_tokens < 1 {
        return Err(CompatError::at(
            "max_tokens",
            "max_tokens must be at least 1",
        ));
    }

    let mut output = Map::new();
    output.insert("model".to_string(), Value::String(options.model.clone()));
    output.insert("messages".to_string(), Value::Array(messages));
    output.insert("max_tokens".to_string(), Value::from(max_tokens));
    output.insert("stream".to_string(), Value::Bool(options.stream));

    if let Some(system) = system {
        output.insert("system".to_string(), Value::Array(system));
    }
    if let Some(tools) = tools {
        output.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(tool_choice) = convert_tool_choice(request.get("tool_choice"))? {
        output.insert("tool_choice".to_string(), tool_choice);
    }
    if let Some(stop) = convert_stop(request.get("stop"))? {
        output.insert("stop_sequences".to_string(), stop);
    }

    copy_sampling_parameters(request, &options.model, &mut output)?;
    copy_anthropic_extensions(request, &mut output)?;
    collect_parameter_warnings(request, &mut warnings);
    deduplicate_warnings(&mut warnings);

    Ok(ConvertedRequest {
        body: Value::Object(output),
        warnings,
    })
}

fn reject_premium_features(request: &Map<String, Value>) -> Result<(), CompatError> {
    if request.get("speed").and_then(Value::as_str) == Some("fast") {
        return Err(CompatError::at(
            "speed",
            "speed=fast is not supported through Chat Completions",
        ));
    }
    if request
        .get("service_tier")
        .and_then(Value::as_str)
        .is_some_and(|tier| tier != "standard_only")
    {
        return Err(CompatError::at(
            "service_tier",
            "only service_tier=standard_only is supported",
        ));
    }
    for parameter in ["inference_geo", "mcp_servers", "container"] {
        if request.contains_key(parameter) {
            return Err(CompatError::at(
                parameter,
                format!("{parameter} is not supported through Chat Completions"),
            ));
        }
    }
    if contains_one_hour_cache_control(&Value::Object(request.clone())) {
        return Err(CompatError::at(
            "cache_control",
            "one-hour prompt caching is not supported yet",
        ));
    }
    Ok(())
}

fn contains_one_hour_cache_control(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object.get("cache_control").is_some_and(|cache_control| {
                cache_control.get("ttl").and_then(Value::as_str) == Some("1h")
            }) || object.values().any(contains_one_hour_cache_control)
        }
        Value::Array(values) => values.iter().any(contains_one_hour_cache_control),
        _ => false,
    }
}

fn integer_parameter(
    request: &Map<String, Value>,
    parameter: &str,
) -> Result<Option<i64>, CompatError> {
    match request.get(parameter).filter(|value| !value.is_null()) {
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| CompatError::at(parameter, format!("{parameter} must be an integer"))),
        None => Ok(None),
    }
}

fn convert_messages(
    messages: &[Value],
    warnings: &mut Vec<ConversionWarning>,
) -> Result<(Option<Vec<Value>>, Vec<Value>), CompatError> {
    let mut system = Vec::new();
    let mut turns: Vec<Value> = Vec::new();

    for (index, message) in messages.iter().enumerate() {
        let path = format!("messages[{index}]");
        let object = message
            .as_object()
            .ok_or_else(|| CompatError::at(&path, format!("{path} must be an object")))?;
        let role = object
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| CompatError::at(format!("{path}.role"), "message role is required"))?;

        warn_unknown_message_fields(object, &path, warnings);

        match role {
            "system" | "developer" => {
                let content = object.get("content").unwrap_or(&Value::Null);
                let mut blocks = convert_content(content, ContentMode::System, &path)?;
                apply_message_cache_control(object, &mut blocks);
                system.extend(blocks);
            }
            "user" => {
                let content = object.get("content").unwrap_or(&Value::Null);
                let mut blocks = convert_content(content, ContentMode::User, &path)?;
                apply_message_cache_control(object, &mut blocks);
                if blocks.is_empty() {
                    blocks.push(text_block("", None));
                }
                push_or_merge_turn(&mut turns, "user", blocks);
            }
            "assistant" => {
                let content = object.get("content").unwrap_or(&Value::Null);
                let mut blocks = convert_content(content, ContentMode::Assistant, &path)?;
                blocks.extend(convert_tool_calls(object.get("tool_calls"), &path)?);
                apply_message_cache_control(object, &mut blocks);
                if blocks.is_empty() {
                    blocks.push(text_block("", None));
                }
                push_or_merge_turn(&mut turns, "assistant", blocks);
            }
            "tool" => {
                let tool_use_id = object
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        CompatError::at(
                            format!("{path}.tool_call_id"),
                            "tool messages require tool_call_id",
                        )
                    })?;
                let content = object.get("content").unwrap_or(&Value::Null);
                let text_blocks = convert_content(content, ContentMode::ToolResult, &path)?;
                let cache_control = last_cache_control(&text_blocks)
                    .or_else(|| object.get("cache_control").cloned());
                let text = text_blocks
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n");
                let mut block = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": text,
                });
                if let Some(cache_control) = cache_control {
                    block
                        .as_object_mut()
                        .expect("tool result is an object")
                        .insert("cache_control".to_string(), cache_control);
                }
                push_or_merge_turn(&mut turns, "user", vec![block]);
            }
            other => {
                return Err(CompatError::at(
                    format!("{path}.role"),
                    format!("unsupported message role: {other}"),
                ));
            }
        }
    }

    Ok(((!system.is_empty()).then_some(system), turns))
}

#[derive(Clone, Copy)]
enum ContentMode {
    System,
    User,
    Assistant,
    ToolResult,
}

fn convert_content(
    content: &Value,
    mode: ContentMode,
    message_path: &str,
) -> Result<Vec<Value>, CompatError> {
    match content {
        Value::Null => Ok(Vec::new()),
        Value::String(text) => Ok(vec![text_block(text, None)]),
        Value::Array(parts) => parts
            .iter()
            .enumerate()
            .map(|(index, part)| convert_content_part(part, mode, message_path, index))
            .collect(),
        _ => Err(CompatError::at(
            format!("{message_path}.content"),
            "message content must be a string, array, or null",
        )),
    }
}

fn convert_content_part(
    part: &Value,
    mode: ContentMode,
    message_path: &str,
    index: usize,
) -> Result<Value, CompatError> {
    let path = format!("{message_path}.content[{index}]");
    let object = part
        .as_object()
        .ok_or_else(|| CompatError::at(&path, format!("{path} must be an object")))?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| CompatError::at(format!("{path}.type"), "content type is required"))?;
    let cache_control = object.get("cache_control").cloned();

    match kind {
        "text" | "input_text" => {
            let text = object
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| CompatError::at(format!("{path}.text"), "text must be a string"))?;
            Ok(text_block(text, cache_control))
        }
        "image_url" | "input_image" if matches!(mode, ContentMode::User) => {
            convert_image_part(object, cache_control, &path)
        }
        "image_url" | "input_image" => Err(CompatError::at(
            format!("{path}.type"),
            "images are supported only in user messages",
        )),
        other => Err(CompatError::at(
            format!("{path}.type"),
            format!("unsupported content type: {other}"),
        )),
    }
}

fn text_block(text: &str, cache_control: Option<Value>) -> Value {
    let mut block = Map::new();
    block.insert("type".to_string(), Value::String("text".to_string()));
    block.insert("text".to_string(), Value::String(text.to_string()));
    if let Some(cache_control) = cache_control {
        block.insert("cache_control".to_string(), cache_control);
    }
    Value::Object(block)
}

fn convert_image_part(
    object: &Map<String, Value>,
    cache_control: Option<Value>,
    path: &str,
) -> Result<Value, CompatError> {
    let image_url = object
        .get("image_url")
        .ok_or_else(|| CompatError::at(format!("{path}.image_url"), "image_url is required"))?;
    let url = image_url
        .as_str()
        .or_else(|| image_url.get("url").and_then(Value::as_str))
        .ok_or_else(|| {
            CompatError::at(
                format!("{path}.image_url"),
                "image_url must be a string or an object with url",
            )
        })?;

    let source = if let Some(data_url) = url.strip_prefix("data:") {
        let (media_type, data) = data_url.split_once(";base64,").ok_or_else(|| {
            CompatError::at(
                format!("{path}.image_url"),
                "image data URLs must use base64 encoding",
            )
        })?;
        json!({"type": "base64", "media_type": media_type, "data": data})
    } else {
        json!({"type": "url", "url": url})
    };

    let mut block = json!({"type": "image", "source": source});
    if let Some(cache_control) = cache_control {
        block
            .as_object_mut()
            .expect("image block is an object")
            .insert("cache_control".to_string(), cache_control);
    }
    Ok(block)
}

fn convert_tool_calls(tool_calls: Option<&Value>, path: &str) -> Result<Vec<Value>, CompatError> {
    let Some(tool_calls) = tool_calls.filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let calls = tool_calls.as_array().ok_or_else(|| {
        CompatError::at(format!("{path}.tool_calls"), "tool_calls must be an array")
    })?;
    calls
        .iter()
        .enumerate()
        .map(|(index, call)| {
            let call_path = format!("{path}.tool_calls[{index}]");
            let object = call.as_object().ok_or_else(|| {
                CompatError::at(&call_path, format!("{call_path} must be an object"))
            })?;
            if object.get("type").and_then(Value::as_str) != Some("function") {
                return Err(CompatError::at(
                    format!("{call_path}.type"),
                    "only function tool calls are supported",
                ));
            }
            let id = object
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    CompatError::at(format!("{call_path}.id"), "tool call id is required")
                })?;
            let function = object
                .get("function")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    CompatError::at(
                        format!("{call_path}.function"),
                        "tool call function is required",
                    )
                })?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    CompatError::at(
                        format!("{call_path}.function.name"),
                        "tool call function name is required",
                    )
                })?;
            let input = match function.get("arguments") {
                Some(Value::String(arguments)) => {
                    serde_json::from_str(arguments).map_err(|_| {
                        CompatError::at(
                            format!("{call_path}.function.arguments"),
                            "tool call arguments must contain valid JSON",
                        )
                    })?
                }
                Some(Value::Object(arguments)) => Value::Object(arguments.clone()),
                Some(_) => {
                    return Err(CompatError::at(
                        format!("{call_path}.function.arguments"),
                        "tool call arguments must be a JSON string or object",
                    ));
                }
                None => json!({}),
            };
            let mut block = json!({"type": "tool_use", "id": id, "name": name, "input": input});
            if let Some(cache_control) = object.get("cache_control").cloned() {
                block
                    .as_object_mut()
                    .expect("tool use is an object")
                    .insert("cache_control".to_string(), cache_control);
            }
            Ok(block)
        })
        .collect()
}

fn push_or_merge_turn(turns: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if let Some(last) = turns.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            if let Some(content) = last.get_mut("content").and_then(Value::as_array_mut) {
                content.extend(blocks);
                return;
            }
        }
    }
    turns.push(json!({"role": role, "content": blocks}));
}

fn apply_message_cache_control(object: &Map<String, Value>, blocks: &mut [Value]) {
    let Some(cache_control) = object.get("cache_control").cloned() else {
        return;
    };
    if let Some(last) = blocks.last_mut().and_then(Value::as_object_mut) {
        last.insert("cache_control".to_string(), cache_control);
    }
}

fn last_cache_control(blocks: &[Value]) -> Option<Value> {
    blocks
        .iter()
        .rev()
        .find_map(|block| block.get("cache_control").cloned())
}

fn convert_tools(
    tools: Option<&Value>,
    warnings: &mut Vec<ConversionWarning>,
) -> Result<Option<Vec<Value>>, CompatError> {
    let Some(tools) = tools.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let tools = tools
        .as_array()
        .ok_or_else(|| CompatError::at("tools", "tools must be an array"))?;
    let converted = tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let path = format!("tools[{index}]");
            let object = tool
                .as_object()
                .ok_or_else(|| CompatError::at(&path, format!("{path} must be an object")))?;
            if object.get("type").and_then(Value::as_str) != Some("function") {
                return Err(CompatError::at(
                    format!("{path}.type"),
                    "only client-executed function tools are supported",
                ));
            }
            let function = object
                .get("function")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    CompatError::at(format!("{path}.function"), "tool function is required")
                })?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    CompatError::at(
                        format!("{path}.function.name"),
                        "tool function name is required",
                    )
                })?;
            let input_schema = function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            let mut converted = Map::new();
            converted.insert("name".to_string(), Value::String(name.to_string()));
            converted.insert("input_schema".to_string(), input_schema);
            if let Some(description) = function.get("description").cloned() {
                converted.insert("description".to_string(), description);
            }
            if let Some(cache_control) = object
                .get("cache_control")
                .or_else(|| function.get("cache_control"))
                .cloned()
            {
                converted.insert("cache_control".to_string(), cache_control);
            }
            for key in object.keys() {
                if !matches!(key.as_str(), "type" | "function" | "cache_control") {
                    warnings.push(ConversionWarning {
                        parameter: format!("{path}.{key}"),
                        reason: "not supported by the Anthropic tool schema",
                    });
                }
            }
            Ok(Value::Object(converted))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(converted))
}

fn convert_tool_choice(tool_choice: Option<&Value>) -> Result<Option<Value>, CompatError> {
    let Some(choice) = tool_choice.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    match choice {
        Value::String(value) => match value.as_str() {
            "auto" => Ok(Some(json!({"type": "auto"}))),
            "required" => Ok(Some(json!({"type": "any"}))),
            "none" => Ok(None),
            other => Err(CompatError::at(
                "tool_choice",
                format!("unsupported tool_choice: {other}"),
            )),
        },
        Value::Object(object) => {
            let name = object
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    CompatError::at(
                        "tool_choice",
                        "named tool_choice must contain function.name",
                    )
                })?;
            Ok(Some(json!({"type": "tool", "name": name})))
        }
        _ => Err(CompatError::at(
            "tool_choice",
            "tool_choice must be a string or object",
        )),
    }
}

fn convert_stop(stop: Option<&Value>) -> Result<Option<Value>, CompatError> {
    let Some(stop) = stop.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    match stop {
        Value::String(value) => Ok(Some(Value::Array(vec![Value::String(value.clone())]))),
        Value::Array(values) if values.iter().all(Value::is_string) => {
            Ok(Some(Value::Array(values.clone())))
        }
        _ => Err(CompatError::at(
            "stop",
            "stop must be a string or an array of strings",
        )),
    }
}

fn copy_sampling_parameters(
    request: &Map<String, Value>,
    model: &str,
    output: &mut Map<String, Value>,
) -> Result<(), CompatError> {
    if MODELS_REJECTING_SAMPLING
        .iter()
        .any(|fragment| model.contains(fragment))
    {
        return Ok(());
    }

    if let Some(temperature) = request.get("temperature").filter(|value| !value.is_null()) {
        let value = temperature
            .as_f64()
            .ok_or_else(|| CompatError::at("temperature", "temperature must be a number"))?;
        if !(0.0..=2.0).contains(&value) {
            return Err(CompatError::at(
                "temperature",
                "temperature must be between 0 and 2",
            ));
        }
        output.insert(
            "temperature".to_string(),
            Value::from(value.clamp(0.0, 1.0)),
        );
    } else if let Some(top_p) = request.get("top_p").filter(|value| !value.is_null()) {
        let value = top_p
            .as_f64()
            .ok_or_else(|| CompatError::at("top_p", "top_p must be a number"))?;
        if !(0.0..=1.0).contains(&value) {
            return Err(CompatError::at("top_p", "top_p must be between 0 and 1"));
        }
        output.insert("top_p".to_string(), Value::from(value));
    }
    Ok(())
}

fn copy_anthropic_extensions(
    request: &Map<String, Value>,
    output: &mut Map<String, Value>,
) -> Result<(), CompatError> {
    for key in [
        "thinking",
        "reasoning_effort",
        "output_config",
        "cache_control",
    ] {
        if let Some(value) = request.get(key).filter(|value| !value.is_null()) {
            output.insert(key.to_string(), value.clone());
        }
    }
    if request.get("service_tier").and_then(Value::as_str) == Some("standard_only") {
        output.insert(
            "service_tier".to_string(),
            Value::String("standard_only".to_string()),
        );
    }
    Ok(())
}

fn collect_parameter_warnings(request: &Map<String, Value>, warnings: &mut Vec<ConversionWarning>) {
    const MAPPED: &[&str] = &[
        "model",
        "messages",
        "max_tokens",
        "max_completion_tokens",
        "temperature",
        "top_p",
        "stop",
        "stream",
        "tools",
        "tool_choice",
        "thinking",
        "reasoning_effort",
        "output_config",
        "cache_control",
        "service_tier",
    ];
    const INTENTIONALLY_DROPPED: &[&str] = &[
        "frequency_penalty",
        "presence_penalty",
        "logit_bias",
        "logprobs",
        "top_logprobs",
        "seed",
        "user",
        "parallel_tool_calls",
        "metadata",
        "store",
        "stream_options",
        "modalities",
        "response_format",
        "n",
    ];

    for key in request.keys() {
        if MAPPED.contains(&key.as_str()) || request.get(key).is_some_and(Value::is_null) {
            continue;
        }
        warnings.push(ConversionWarning {
            parameter: key.clone(),
            reason: if INTENTIONALLY_DROPPED.contains(&key.as_str()) {
                "has no equivalent in the Anthropic Messages API"
            } else {
                "unknown Chat Completions parameter; not forwarded"
            },
        });
    }
}

fn warn_unknown_message_fields(
    object: &Map<String, Value>,
    path: &str,
    warnings: &mut Vec<ConversionWarning>,
) {
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "role" | "content" | "name" | "tool_call_id" | "tool_calls" | "cache_control"
        ) {
            warnings.push(ConversionWarning {
                parameter: format!("{path}.{key}"),
                reason: "unknown message field; not forwarded",
            });
        }
    }
}

fn deduplicate_warnings(warnings: &mut Vec<ConversionWarning>) {
    let mut seen = BTreeSet::new();
    warnings.retain(|warning| seen.insert(warning.parameter.clone()));
}

fn clamp_cache_breakpoints(
    tools: &mut Option<Vec<Value>>,
    system: &mut Option<Vec<Value>>,
    messages: &mut [Value],
    automatic_cache: bool,
) {
    // Anthropic's top-level automatic cache control consumes one of the same
    // four slots as explicit block breakpoints. It targets the latest eligible
    // block, so preserve it and remove the oldest explicit markers first.
    let count = usize::from(automatic_cache)
        + tools
            .iter()
            .flatten()
            .chain(system.iter().flatten())
            .chain(messages.iter().flat_map(message_blocks))
            .filter(|block| block.get("cache_control").is_some())
            .count();
    let mut remove = count.saturating_sub(MAX_CACHE_BREAKPOINTS);
    if remove == 0 {
        return;
    }

    if let Some(tools) = tools {
        remove_oldest_markers(tools, &mut remove);
    }
    if let Some(system) = system {
        remove_oldest_markers(system, &mut remove);
    }
    for message in messages {
        if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
            remove_oldest_markers(blocks, &mut remove);
        }
    }
}

fn message_blocks(message: &Value) -> impl Iterator<Item = &Value> {
    message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn remove_oldest_markers(blocks: &mut [Value], remaining: &mut usize) {
    for block in blocks {
        if *remaining == 0 {
            return;
        }
        if block
            .as_object_mut()
            .is_some_and(|object| object.remove("cache_control").is_some())
        {
            *remaining -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> ConvertOptions {
        ConvertOptions {
            model: "claude-sonnet-upstream".to_string(),
            stream: false,
        }
    }

    fn convert(input: Value) -> ConvertedRequest {
        convert_openai_request(&input, &options()).expect("conversion should succeed")
    }

    #[test]
    fn converts_basic_request_and_accumulates_system_messages() {
        let converted = convert(json!({
            "model": "anthropic/claude-sonnet",
            "messages": [
                {"role": "system", "content": "first"},
                {"role": "developer", "content": [{"type": "text", "text": "second"}]},
                {"role": "user", "content": "hello"}
            ],
            "max_completion_tokens": 123,
            "temperature": 1.7,
            "top_p": 0.5
        }));

        assert_eq!(converted.body["model"], "claude-sonnet-upstream");
        assert_eq!(converted.body["max_tokens"], 123);
        assert_eq!(converted.body["temperature"], 1.0);
        assert!(converted.body.get("top_p").is_none());
        assert_eq!(converted.body["system"].as_array().unwrap().len(), 2);
        assert_eq!(converted.body["messages"][0]["content"][0]["type"], "text");
    }

    #[test]
    fn preserves_tool_loop_cache_markers_and_canonical_shapes() {
        let request = json!({
            "model": "anthropic/claude-sonnet",
            "messages": [
                {"role": "user", "content": [{"type":"text", "text":"go"}]},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id":"toolu_1", "type":"function",
                    "function":{"name":"lookup", "arguments":"{\"q\":\"x\"}"}
                }]},
                {"role": "tool", "tool_call_id":"toolu_1", "content":[{
                    "type":"text", "text":"result", "cache_control":{"type":"ephemeral"}
                }]},
                {"role":"user", "content":"continue"}
            ]
        });
        let converted = convert(request);
        let messages = converted.body["messages"].as_array().unwrap();
        let tool_result = &messages[2]["content"][0];
        assert_eq!(tool_result["type"], "tool_result");
        assert_eq!(tool_result["cache_control"]["type"], "ephemeral");
        assert!(messages.iter().all(|message| message["content"].is_array()));
    }

    #[test]
    fn canonical_shape_does_not_depend_on_cache_marker_location() {
        let base = json!({
            "model":"m",
            "messages":[
                {"role":"user","content":[{"type":"text","text":"a"}]},
                {"role":"assistant","content":[{"type":"text","text":"b"}]}
            ]
        });
        let mut marked = base.clone();
        marked["messages"][0]["content"][0]["cache_control"] = json!({"type":"ephemeral"});

        let plain = convert(base).body;
        let mut cached = convert(marked).body;
        cached["messages"][0]["content"][0]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        assert_eq!(plain, cached);
    }

    #[test]
    fn keeps_only_four_newest_cache_breakpoints() {
        let content = (0..6)
            .map(|index| {
                json!({
                    "type":"text",
                    "text":index.to_string(),
                    "cache_control":{"type":"ephemeral"}
                })
            })
            .collect::<Vec<_>>();
        let converted = convert(json!({
            "model":"m",
            "messages":[{"role":"user","content":content}]
        }));
        let blocks = converted.body["messages"][0]["content"].as_array().unwrap();
        assert!(blocks[0].get("cache_control").is_none());
        assert!(blocks[1].get("cache_control").is_none());
        assert_eq!(
            blocks
                .iter()
                .filter(|block| block.get("cache_control").is_some())
                .count(),
            4
        );
    }

    #[test]
    fn automatic_cache_control_uses_one_of_the_four_breakpoint_slots() {
        let content = (0..4)
            .map(|index| {
                json!({
                    "type":"text",
                    "text":index.to_string(),
                    "cache_control":{"type":"ephemeral"}
                })
            })
            .collect::<Vec<_>>();
        let converted = convert(json!({
            "model":"m",
            "cache_control":{"type":"ephemeral"},
            "messages":[{"role":"user","content":content}]
        }));
        let blocks = converted.body["messages"][0]["content"].as_array().unwrap();
        assert!(blocks[0].get("cache_control").is_none());
        assert_eq!(
            blocks
                .iter()
                .filter(|block| block.get("cache_control").is_some())
                .count(),
            3
        );
        assert_eq!(converted.body["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn converts_remote_and_inline_images_without_touching_payload() {
        let converted = convert(json!({
            "model":"m",
            "messages":[{"role":"user","content":[
                {"type":"image_url","image_url":{"url":"https://example.test/a.png"}},
                {"type":"image_url","image_url":"data:image/png;base64,AAEC"}
            ]}]
        }));
        let blocks = converted.body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(
            blocks[0]["source"],
            json!({"type":"url","url":"https://example.test/a.png"})
        );
        assert_eq!(
            blocks[1]["source"],
            json!({"type":"base64","media_type":"image/png","data":"AAEC"})
        );
    }

    #[test]
    fn forwards_supported_anthropic_extensions_and_warns_on_every_drop() {
        let converted = convert(json!({
            "model":"m",
            "messages":[{"role":"user","content":"hi","future_message_field":true}],
            "thinking":{"type":"enabled","budget_tokens":1024},
            "output_config":{"effort":"high"},
            "cache_control":{"type":"ephemeral"},
            "presence_penalty":0.2,
            "future_top_level":{"x":1}
        }));
        assert_eq!(converted.body["thinking"]["budget_tokens"], 1024);
        assert_eq!(converted.body["output_config"]["effort"], "high");
        assert_eq!(converted.body["cache_control"]["type"], "ephemeral");
        let parameters = converted
            .warnings
            .iter()
            .map(|warning| warning.parameter.as_str())
            .collect::<Vec<_>>();
        assert!(parameters.contains(&"presence_penalty"));
        assert!(parameters.contains(&"future_top_level"));
        assert!(parameters.contains(&"messages[0].future_message_field"));
    }

    #[test]
    fn rejects_server_tools_and_one_hour_cache_without_network() {
        let server_tool = json!({
            "model":"m",
            "messages":[{"role":"user","content":"hi"}],
            "tools":[{"type":"web_search_20250305","name":"web_search"}]
        });
        assert_eq!(
            convert_openai_request(&server_tool, &options())
                .unwrap_err()
                .parameter
                .as_deref(),
            Some("tools[0].type")
        );

        let long_cache = json!({
            "model":"m",
            "messages":[{"role":"user","content":[{
                "type":"text","text":"hi",
                "cache_control":{"type":"ephemeral","ttl":"1h"}
            }]}]
        });
        assert_eq!(
            convert_openai_request(&long_cache, &options())
                .unwrap_err()
                .parameter
                .as_deref(),
            Some("cache_control")
        );
    }

    #[test]
    fn rejects_unknown_content_instead_of_silently_dropping_it() {
        let error = convert_openai_request(
            &json!({
                "model":"m",
                "messages":[{"role":"user","content":[{"type":"input_audio","data":"secret"}]}]
            }),
            &options(),
        )
        .unwrap_err();
        assert_eq!(
            error.parameter.as_deref(),
            Some("messages[0].content[0].type")
        );
    }
}
