use super::{invalid, RouteResult};
use crate::models::ChatCompletionRequest;
use serde_json::{json, Value};

pub(super) fn translate(
    body: &Value,
    model: &str,
    supports_effort: bool,
) -> RouteResult<ChatCompletionRequest> {
    let mut messages = Vec::new();
    if let Some(system) = body.get("system") {
        let text = match system.as_str() {
            Some(text) => text.to_owned(),
            None => blocks(system)?
                .iter()
                .map(|b| {
                    if b["type"] != "text" {
                        return Err(invalid("Expected a text block in system"));
                    }
                    string(b, "text")
                })
                .collect::<RouteResult<Vec<_>>>()?
                .join("\n\n"),
        };
        if !text.is_empty() {
            messages.push(json!({"role":"system","content":text}));
        }
    }
    let turns = blocks(&body["messages"])?;
    if turns.is_empty() {
        return Err(invalid("messages must contain at least one turn"));
    }
    for turn in turns {
        let role = string(turn, "role")?;
        if !matches!(role, "user" | "assistant") {
            return Err(invalid("Expected a user or assistant role"));
        }
        let content = &turn["content"];
        if content.is_string() {
            messages.push(json!({"role":role,"content":content}));
        } else if role == "assistant" {
            messages.push(assistant(blocks(content)?)?);
        } else {
            user(blocks(content)?, &mut messages)?;
        }
    }
    let max_tokens = body["max_tokens"]
        .as_i64()
        .filter(|n| *n > 0)
        .ok_or_else(|| invalid("max_tokens must be a positive integer"))?;
    let mut chat = json!({"model":model, "messages":messages, "max_tokens":max_tokens});
    for (src, dst) in [
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("top_k", "top_k"),
        ("stream", "stream"),
        ("stop_sequences", "stop"),
    ] {
        if let Some(v) = body.get(src) {
            chat[dst] = v.clone();
        }
    }
    if chat["stream"] == true {
        chat["stream_options"] = json!({"include_usage":true});
    }

    if let Some(tools) = body.get("tools") {
        let functions = blocks(tools)?
            .iter()
            .map(|tool| {
                let name = string(tool, "name")?;
                if name.is_empty() || !tool["input_schema"].is_object() {
                    return Err(invalid("Each tool requires a name and input_schema"));
                }
                let mut function = json!({"name":name,"parameters":tool["input_schema"]});
                if tool["strict"] == true {
                    return Err(invalid(
                        "Strict tool argument enforcement is unavailable for this model",
                    ));
                }
                if let Some(description) = tool.get("description") {
                    function["description"] = description.clone();
                }
                Ok(json!({"type":"function","function":function}))
            })
            .collect::<RouteResult<Vec<_>>>()?;
        if !functions.is_empty() {
            chat["tools"] = functions.into();
        }
    }
    if let Some(choice) = body.get("tool_choice") {
        chat["tool_choice"] = match string(choice, "type")? {
            "auto" => json!("auto"),
            "any" => json!("required"),
            "none" => json!("none"),
            "tool" => json!({"type":"function","function":{"name":string(choice,"name")?}}),
            _ => return Err(invalid("Unsupported tool_choice")),
        };
        if let Some(disable) = choice.get("disable_parallel_tool_use") {
            chat["parallel_tool_calls"] = json!(!disable
                .as_bool()
                .ok_or_else(|| invalid("disable_parallel_tool_use must be a boolean"))?);
        }
    }
    if let Some(format) = body.pointer("/output_config/format") {
        if format["type"] != "json_schema" || !format["schema"].is_object() {
            return Err(invalid("Unsupported output_config.format"));
        }
        chat["response_format"] = json!({"type":"json_schema","json_schema":{"name":"response","schema":format["schema"],"strict":true}});
    }
    if let Some(effort) = reasoning_effort(body)?.filter(|_| supports_effort) {
        chat["reasoning_effort"] = json!(effort);
    }
    if let Some(management) = body.get("context_management") {
        if !blocks(&management["edits"])?
            .iter()
            .all(|edit| edit["type"] == "clear_thinking_20251015" && edit["keep"] == "all")
        {
            return Err(invalid(
                "Server-side context edits are unavailable for this model",
            ));
        }
    }
    let request: ChatCompletionRequest =
        serde_json::from_value(chat).map_err(|_| invalid("Invalid Messages request parameters"))?;
    request
        .validate_request()
        .map_err(|e| invalid(e.error.message))?;
    Ok(request)
}

fn reasoning_effort(body: &Value) -> RouteResult<Option<&'static str>> {
    let explicit = match body.pointer("/output_config/effort") {
        None => None,
        Some(value) => Some(match value.as_str() {
            Some("low") => "low",
            Some("medium") => "medium",
            Some("high" | "xhigh" | "max") => "high",
            _ => return Err(invalid("Unsupported output_config.effort")),
        }),
    };
    let Some(thinking) = body.get("thinking") else {
        return Ok(explicit);
    };
    match string(thinking, "type")? {
        "disabled" => Ok(Some("none")),
        "adaptive" => Ok(explicit),
        "enabled" => {
            let budget = thinking["budget_tokens"]
                .as_u64()
                .filter(|v| *v > 0)
                .ok_or_else(|| invalid("thinking.budget_tokens must be a positive integer"))?;
            let effort = match budget {
                0..4000 => "low",
                4000..16000 => "medium",
                _ => "high",
            };
            Ok(explicit.or(Some(effort)))
        }
        _ => Err(invalid("Unsupported thinking type")),
    }
}

fn blocks(value: &Value) -> RouteResult<&[Value]> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| invalid("Expected an array"))
}

fn string<'a>(value: &'a Value, field: &str) -> RouteResult<&'a str> {
    value[field]
        .as_str()
        .ok_or_else(|| invalid(format!("Expected a string for {field}")))
}

fn assistant(content: &[Value]) -> RouteResult<Value> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls = Vec::new();
    for block in content {
        match string(block, "type")? {
            "text" => text.push_str(string(block, "text")?),
            "thinking" => reasoning.push_str(string(block, "thinking")?),
            "tool_use" => {
                if !block["input"].is_object() {
                    return Err(invalid("tool_use input must be an object"));
                }
                calls.push(
                    json!({"id":string(block,"id")?,"type":"function","function":{
                        "name":string(block,"name")?, "arguments":block["input"].to_string()
                    }}),
                );
            }
            _ => {
                return Err(invalid(
                    "Unsupported assistant content block for a Chat Completions model",
                ))
            }
        }
    }
    let mut message = json!({"role":"assistant","content":text});
    if !reasoning.is_empty() {
        message["reasoning_content"] = reasoning.into();
    }
    if !calls.is_empty() {
        message["tool_calls"] = calls.into();
    }
    Ok(message)
}

fn user(content: &[Value], messages: &mut Vec<Value>) -> RouteResult<()> {
    let mut parts = Vec::new();
    for block in content {
        if block["type"] == "tool_result" {
            let id = string(block, "tool_use_id")?;
            let mut text = Vec::new();
            if let Some(result) = block.get("content") {
                if let Some(value) = result.as_str() {
                    text.push(value.to_owned());
                } else {
                    for item in blocks(result)? {
                        match string(item, "type")? {
                            "text" => text.push(string(item, "text")?.to_owned()),
                            "tool_reference" => text.push(string(item, "tool_name")?.to_owned()),
                            "image" => {
                                parts.push(json!({"type":"text","text":format!("Image from tool call {id}:")}));
                                parts.push(user_part(item)?);
                            }
                            _ => return Err(invalid("Unsupported tool_result content block")),
                        }
                    }
                }
            }
            if block["is_error"] == true {
                text.insert(0, "Tool error:".to_owned());
            }
            messages.push(json!({"role":"tool","tool_call_id":id,"content":text.join("\n")}));
        } else {
            parts.push(user_part(block)?);
        }
    }
    if !parts.is_empty() {
        let content = if parts.iter().all(|part| part["type"] == "text") {
            json!(parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"))
        } else {
            parts.into()
        };
        messages.push(json!({"role":"user","content":content}));
    } else if content.is_empty() {
        return Err(invalid("User content must contain at least one block"));
    }
    Ok(())
}

fn user_part(block: &Value) -> RouteResult<Value> {
    match string(block, "type")? {
        "text" => Ok(json!({"type":"text","text":string(block,"text")?})),
        "image" => {
            let source = &block["source"];
            let url = match string(source, "type")? {
                "url" => string(source, "url")?.to_owned(),
                "base64" => format!(
                    "data:{};base64,{}",
                    string(source, "media_type")?,
                    string(source, "data")?
                ),
                _ => return Err(invalid("Unsupported image source")),
            };
            Ok(json!({"type":"image_url","image_url":{"url":url}}))
        }
        _ => Err(invalid(
            "Unsupported user content block for a Chat Completions model",
        )),
    }
}
