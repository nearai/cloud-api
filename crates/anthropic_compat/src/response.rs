use crate::{CompatError, ConversionWarning};
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseOptions {
    pub model: String,
    pub created: i64,
    pub strip_json_fence: bool,
}

/// Convert a complete Anthropic Messages response into an OpenAI Chat
/// Completions response. The function is pure and returns wire JSON rather
/// than depending on Cloud API response models.
pub fn convert_anthropic_response(
    input: &Value,
    options: &ResponseOptions,
) -> Result<(Value, Vec<ConversionWarning>), CompatError> {
    let response = input
        .as_object()
        .ok_or_else(|| CompatError::request("Anthropic response must be a JSON object"))?;
    let id = response
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| CompatError::at("id", "Anthropic response id is required"))?;
    let content = response
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| CompatError::at("content", "Anthropic response content must be an array"))?;

    let mut warnings = Vec::new();
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for (index, block) in content.iter().enumerate() {
        let path = format!("content[{index}]");
        let object = block
            .as_object()
            .ok_or_else(|| CompatError::at(&path, format!("{path} must be an object")))?;
        match object.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(value) = object.get("text").and_then(Value::as_str) {
                    text.push_str(value);
                }
            }
            Some("thinking") => {
                if let Some(value) = object.get("thinking").and_then(Value::as_str) {
                    reasoning.push_str(value);
                }
            }
            Some("tool_use") => {
                let tool_id = object.get("id").and_then(Value::as_str).ok_or_else(|| {
                    CompatError::at(format!("{path}.id"), "tool_use id is required")
                })?;
                let name = object.get("name").and_then(Value::as_str).ok_or_else(|| {
                    CompatError::at(format!("{path}.name"), "tool_use name is required")
                })?;
                let arguments = serde_json::to_string(
                    object.get("input").unwrap_or(&Value::Object(Map::new())),
                )
                .map_err(|_| {
                    CompatError::at(format!("{path}.input"), "tool_use input is invalid")
                })?;
                tool_calls.push(json!({
                    "id": tool_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                }));
            }
            Some(kind) => warnings.push(ConversionWarning {
                parameter: path,
                reason: if kind == "redacted_thinking" {
                    "redacted thinking has no OpenAI Chat Completions representation"
                } else {
                    "unknown Anthropic content block; not exposed"
                },
            }),
            None => {
                return Err(CompatError::at(
                    format!("{path}.type"),
                    "Anthropic content block type is required",
                ));
            }
        }
    }

    if options.strip_json_fence {
        text = strip_json_code_fence(&text);
    }

    let usage = token_usage(response.get("usage"))?;
    let stop_reason = map_finish_reason(response.get("stop_reason").and_then(Value::as_str));
    let mut message = Map::new();
    message.insert("role".to_string(), Value::String("assistant".to_string()));
    if !text.is_empty() {
        message.insert("content".to_string(), Value::String(text));
    }
    if !reasoning.is_empty() {
        message.insert("reasoning_content".to_string(), Value::String(reasoning));
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }

    Ok((
        json!({
            "id": id,
            "object": "chat.completion",
            "created": options.created,
            "model": options.model,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": stop_reason,
            }],
            "usage": usage,
        }),
        warnings,
    ))
}

pub(crate) fn token_usage(usage: Option<&Value>) -> Result<Value, CompatError> {
    let usage = usage
        .and_then(Value::as_object)
        .ok_or_else(|| CompatError::at("usage", "Anthropic usage must be an object"))?;
    let input = non_negative_i32(usage, "input_tokens")?;
    let output = non_negative_i32(usage, "output_tokens")?;
    let cache_read = optional_non_negative_i32(usage, "cache_read_input_tokens")?;
    let cache_write = optional_non_negative_i32(usage, "cache_creation_input_tokens")?;
    let prompt = input.saturating_add(cache_read).saturating_add(cache_write);

    let mut result = json!({
        "prompt_tokens": prompt,
        "completion_tokens": output,
        "total_tokens": prompt.saturating_add(output),
    });
    if cache_read > 0 || cache_write > 0 {
        result.as_object_mut().expect("usage is an object").insert(
            "prompt_tokens_details".to_string(),
            json!({
                "cached_tokens": cache_read,
                "cache_creation_tokens": cache_write,
            }),
        );
    }
    Ok(result)
}

fn non_negative_i32(usage: &Map<String, Value>, key: &str) -> Result<i32, CompatError> {
    let value = usage.get(key).and_then(Value::as_i64).ok_or_else(|| {
        CompatError::at(
            format!("usage.{key}"),
            format!("usage.{key} must be an integer"),
        )
    })?;
    i32::try_from(value)
        .ok()
        .filter(|value| *value >= 0)
        .ok_or_else(|| {
            CompatError::at(
                format!("usage.{key}"),
                format!("usage.{key} is out of range"),
            )
        })
}

fn optional_non_negative_i32(usage: &Map<String, Value>, key: &str) -> Result<i32, CompatError> {
    match usage.get(key) {
        Some(_) => non_negative_i32(usage, key),
        None => Ok(0),
    }
}

pub(crate) fn map_finish_reason(reason: Option<&str>) -> Option<&'static str> {
    match reason {
        Some("end_turn" | "stop_sequence" | "pause_turn") => Some("stop"),
        Some("max_tokens" | "model_context_window_exceeded") => Some("length"),
        Some("tool_use") => Some("tool_calls"),
        Some("refusal") => Some("content_filter"),
        Some(_) => Some("stop"),
        None => None,
    }
}

fn strip_json_code_fence(content: &str) -> String {
    let trimmed = content.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return content.to_string();
    };
    let Some(line_end) = rest.find('\n') else {
        return content.to_string();
    };
    let Some(inner) = rest[line_end + 1..].trim_end().strip_suffix("```") else {
        return content.to_string();
    };
    inner.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(content: Value, usage: Value) -> Value {
        json!({
            "id":"msg_1",
            "model":"claude-upstream-dated",
            "content":content,
            "stop_reason":"end_turn",
            "usage":usage,
        })
    }

    fn options(strip_json_fence: bool) -> ResponseOptions {
        ResponseOptions {
            model: "claude-cloud".to_string(),
            created: 123,
            strip_json_fence,
        }
    }

    #[test]
    fn converts_text_tools_reasoning_and_cache_usage() {
        let (converted, warnings) = convert_anthropic_response(
            &json!({
                "id":"msg_1",
                "content":[
                    {"type":"thinking","thinking":"considering"},
                    {"type":"text","text":"done"},
                    {"type":"tool_use","id":"toolu_1","name":"lookup","input":{"q":"x"}}
                ],
                "stop_reason":"tool_use",
                "usage":{
                    "input_tokens":10,
                    "output_tokens":2,
                    "cache_read_input_tokens":3,
                    "cache_creation_input_tokens":4
                }
            }),
            &ResponseOptions {
                model: "claude-cloud".to_string(),
                created: 123,
                strip_json_fence: false,
            },
        )
        .unwrap();
        assert!(warnings.is_empty());
        assert_eq!(converted["choices"][0]["message"]["content"], "done");
        assert_eq!(
            converted["choices"][0]["message"]["reasoning_content"],
            "considering"
        );
        assert_eq!(converted["choices"][0]["finish_reason"], "tool_calls");
        assert!(converted["choices"][0]["message"]["tool_calls"][0]
            .get("index")
            .is_none());
        assert_eq!(converted["usage"]["prompt_tokens"], 17);
        assert_eq!(
            converted["usage"]["prompt_tokens_details"]["cache_creation_tokens"],
            4
        );
    }

    #[test]
    fn production_converter_preserves_model_usage_and_json_regressions() {
        let cases = [
            ("```json\n{\"city\":\"Paris\"}\n```", "{\"city\":\"Paris\"}"),
            ("```\n{\"a\":1}\n```", "{\"a\":1}"),
            ("{\"raw\":true}", "{\"raw\":true}"),
        ];
        for (input, expected) in cases {
            let (converted, _) = convert_anthropic_response(
                &response(
                    json!([{"type":"text","text":input}]),
                    json!({
                        "input_tokens":10,
                        "output_tokens":30,
                        "cache_read_input_tokens":80,
                        "cache_creation_input_tokens":5,
                    }),
                ),
                &options(true),
            )
            .unwrap();
            assert_eq!(converted["model"], "claude-cloud");
            assert_eq!(converted["choices"][0]["message"]["content"], expected);
            assert_eq!(converted["usage"]["prompt_tokens"], 95);
            assert_eq!(converted["usage"]["completion_tokens"], 30);
            assert_eq!(converted["usage"]["total_tokens"], 125);
            assert_eq!(
                converted["usage"]["prompt_tokens_details"]["cached_tokens"],
                80
            );
            assert_eq!(
                converted["usage"]["prompt_tokens_details"]["cache_creation_tokens"],
                5
            );
        }
    }

    #[test]
    fn cache_creation_only_emits_details_and_no_cache_omits_them() {
        let (creation, _) = convert_anthropic_response(
            &response(
                json!([]),
                json!({
                    "input_tokens":10,"output_tokens":3,
                    "cache_read_input_tokens":0,"cache_creation_input_tokens":7,
                }),
            ),
            &options(false),
        )
        .unwrap();
        assert_eq!(creation["usage"]["prompt_tokens"], 17);
        assert_eq!(
            creation["usage"]["prompt_tokens_details"]["cache_creation_tokens"],
            7
        );

        let (uncached, _) = convert_anthropic_response(
            &response(json!([]), json!({"input_tokens":100,"output_tokens":20})),
            &options(false),
        )
        .unwrap();
        assert_eq!(uncached["usage"]["prompt_tokens"], 100);
        assert!(uncached["usage"].get("prompt_tokens_details").is_none());
    }

    #[test]
    fn unknown_terminal_reason_falls_back_to_stop() {
        let mut input = response(json!([]), json!({"input_tokens":1,"output_tokens":1}));
        input["stop_reason"] = json!("future_reason");
        let (converted, _) = convert_anthropic_response(&input, &options(false)).unwrap();
        assert_eq!(converted["choices"][0]["finish_reason"], "stop");
    }
}
