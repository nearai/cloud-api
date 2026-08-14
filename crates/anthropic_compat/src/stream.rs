use crate::response::{map_finish_reason, token_usage};
use crate::CompatError;
use serde_json::{json, Value};
use std::collections::HashMap;

/// Pure Anthropic SSE event to OpenAI chunk state machine.
#[derive(Debug, Clone)]
pub struct StreamState {
    model: String,
    created: i64,
    message_id: String,
    input_tokens: i32,
    output_tokens: i32,
    cache_read_tokens: i32,
    cache_creation_tokens: i32,
    tool_calls: HashMap<i64, i64>,
    next_tool_index: i64,
}

impl StreamState {
    pub fn new(model: String, created: i64) -> Self {
        Self {
            model,
            created,
            message_id: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls: HashMap::new(),
            next_tool_index: 0,
        }
    }

    pub fn input_tokens(&self) -> i32 {
        self.input_tokens
    }

    /// Convert one decoded Anthropic SSE `data:` JSON value into at most one
    /// OpenAI chat-completion chunk.
    pub fn convert_event(&mut self, event: &Value) -> Result<Option<Value>, CompatError> {
        let object = event
            .as_object()
            .ok_or_else(|| CompatError::request("Anthropic stream event must be an object"))?;
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| CompatError::at("type", "Anthropic stream event type is required"))?;

        match kind {
            "message_start" => {
                let message = object
                    .get("message")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        CompatError::at("message", "message_start.message is required")
                    })?;
                self.message_id = message
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| CompatError::at("message.id", "message id is required"))?
                    .to_string();
                self.apply_usage(message.get("usage"))?;
                Ok(Some(self.chunk(
                    json!({"role":"assistant"}),
                    None,
                    Some(self.current_usage()?),
                )))
            }
            "content_block_start" => {
                let block_index = required_i64(object.get("index"), "index")?;
                let block = object
                    .get("content_block")
                    .and_then(Value::as_object)
                    .ok_or_else(|| CompatError::at("content_block", "content_block is required"))?;
                if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                    return Ok(None);
                }
                let id = block.get("id").and_then(Value::as_str).ok_or_else(|| {
                    CompatError::at("content_block.id", "tool_use id is required")
                })?;
                let name = block.get("name").and_then(Value::as_str).ok_or_else(|| {
                    CompatError::at("content_block.name", "tool_use name is required")
                })?;
                let tool_index = self.next_tool_index;
                self.next_tool_index += 1;
                self.tool_calls.insert(block_index, tool_index);
                Ok(Some(self.chunk(
                    json!({"tool_calls":[{
                        "index":tool_index,
                        "id":id,
                        "type":"function",
                        "function":{"name":name}
                    }]}),
                    None,
                    None,
                )))
            }
            "content_block_delta" => {
                let block_index = required_i64(object.get("index"), "index")?;
                let delta = object
                    .get("delta")
                    .and_then(Value::as_object)
                    .ok_or_else(|| CompatError::at("delta", "content block delta is required"))?;
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => Ok(delta
                        .get("text")
                        .and_then(Value::as_str)
                        .map(|text| self.chunk(json!({"content":text}), None, None))),
                    Some("thinking_delta") => Ok(delta
                        .get("thinking")
                        .and_then(Value::as_str)
                        .map(|thinking| {
                            self.chunk(json!({"reasoning_content":thinking}), None, None)
                        })),
                    Some("input_json_delta") => {
                        let Some(tool_index) = self.tool_calls.get(&block_index).copied() else {
                            return Ok(None);
                        };
                        Ok(delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .map(|arguments| {
                                self.chunk(
                                    json!({"tool_calls":[{
                                        "index":tool_index,
                                        "function":{"arguments":arguments}
                                    }]}),
                                    None,
                                    None,
                                )
                            }))
                    }
                    _ => Ok(None),
                }
            }
            "content_block_stop" => {
                if let Some(index) = object.get("index").and_then(Value::as_i64) {
                    self.tool_calls.remove(&index);
                }
                Ok(None)
            }
            "message_delta" => {
                self.apply_usage(object.get("usage"))?;
                let reason = object
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str);
                Ok(Some(self.chunk(
                    json!({}),
                    map_finish_reason(reason),
                    Some(self.current_usage()?),
                )))
            }
            "error" => {
                let error = object.get("error");
                let error_type = error
                    .and_then(|error| error.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("api_error");
                let message = error
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown upstream error");
                Err(CompatError::upstream(format!(
                    "Anthropic error: {error_type} - {message}"
                )))
            }
            "ping" | "message_stop" => Ok(None),
            _ => Ok(None),
        }
    }

    fn apply_usage(&mut self, usage: Option<&Value>) -> Result<(), CompatError> {
        let usage = usage
            .and_then(Value::as_object)
            .ok_or_else(|| CompatError::at("usage", "Anthropic stream usage is required"))?;
        update_count(
            &mut self.input_tokens,
            usage.get("input_tokens"),
            "input_tokens",
        )?;
        update_count(
            &mut self.output_tokens,
            usage.get("output_tokens"),
            "output_tokens",
        )?;
        update_count(
            &mut self.cache_read_tokens,
            usage.get("cache_read_input_tokens"),
            "cache_read_input_tokens",
        )?;
        update_count(
            &mut self.cache_creation_tokens,
            usage.get("cache_creation_input_tokens"),
            "cache_creation_input_tokens",
        )?;
        Ok(())
    }

    fn current_usage(&self) -> Result<Value, CompatError> {
        token_usage(Some(&json!({
            "input_tokens":self.input_tokens,
            "output_tokens":self.output_tokens,
            "cache_read_input_tokens":self.cache_read_tokens,
            "cache_creation_input_tokens":self.cache_creation_tokens,
        })))
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>, usage: Option<Value>) -> Value {
        json!({
            "id":self.message_id,
            "object":"chat.completion.chunk",
            "created":self.created,
            "model":self.model,
            "choices":[{
                "index":0,
                "delta":delta,
                "finish_reason":finish_reason,
            }],
            "usage":usage,
        })
    }
}

fn required_i64(value: Option<&Value>, parameter: &str) -> Result<i64, CompatError> {
    value
        .and_then(Value::as_i64)
        .ok_or_else(|| CompatError::at(parameter, format!("{parameter} must be an integer")))
}

fn update_count(
    target: &mut i32,
    value: Option<&Value>,
    parameter: &str,
) -> Result<(), CompatError> {
    let Some(value) = value else { return Ok(()) };
    let value = value
        .as_i64()
        .and_then(|value| i32::try_from(value).ok())
        .filter(|value| *value >= 0)
        .ok_or_else(|| CompatError::at(parameter, format!("{parameter} is out of range")))?;
    *target = (*target).max(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_fragmented_event_sequence_with_reasoning_tools_and_usage() {
        let mut state = StreamState::new("claude-cloud".to_string(), 123);
        let start = state
            .convert_event(&json!({
                "type":"message_start",
                "message":{"id":"msg_1","usage":{
                    "input_tokens":10,"output_tokens":0,
                    "cache_read_input_tokens":3,"cache_creation_input_tokens":4
                }}
            }))
            .unwrap()
            .unwrap();
        assert_eq!(start["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(start["usage"]["prompt_tokens"], 17);

        let reasoning = state
            .convert_event(&json!({
                "type":"content_block_delta","index":0,
                "delta":{"type":"thinking_delta","thinking":"hmm"}
            }))
            .unwrap()
            .unwrap();
        assert_eq!(reasoning["choices"][0]["delta"]["reasoning_content"], "hmm");

        state
            .convert_event(&json!({
                "type":"content_block_start","index":1,
                "content_block":{"type":"tool_use","id":"toolu_1","name":"lookup"}
            }))
            .unwrap();
        let arguments = state
            .convert_event(&json!({
                "type":"content_block_delta","index":1,
                "delta":{"type":"input_json_delta","partial_json":"{\"q\":"}
            }))
            .unwrap()
            .unwrap();
        assert_eq!(
            arguments["choices"][0]["delta"]["tool_calls"][0]["index"],
            0
        );

        let finish = state
            .convert_event(&json!({
                "type":"message_delta",
                "delta":{"stop_reason":"tool_use"},
                "usage":{"output_tokens":2}
            }))
            .unwrap()
            .unwrap();
        assert_eq!(finish["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            finish["usage"]["prompt_tokens_details"]["cache_creation_tokens"],
            4
        );

        let error = state
            .convert_event(&json!({
                "type":"error",
                "error":{"type":"overloaded_error","message":"try again"}
            }))
            .unwrap_err();
        assert_eq!(
            error.message,
            "Anthropic error: overloaded_error - try again"
        );
        assert_eq!(error.kind, crate::CompatErrorKind::Upstream);
    }

    #[test]
    fn cumulative_usage_does_not_regress_and_unknown_stop_reason_stops() {
        let mut state = StreamState::new("claude-cloud".to_string(), 123);
        state
            .convert_event(&json!({
                "type":"message_start",
                "message":{"id":"msg_1","usage":{
                    "input_tokens":10,"output_tokens":0,
                    "cache_read_input_tokens":3,"cache_creation_input_tokens":4
                }}
            }))
            .unwrap();

        let finish = state
            .convert_event(&json!({
                "type":"message_delta",
                "delta":{"stop_reason":"future_reason"},
                "usage":{
                    "input_tokens":0,"output_tokens":2,
                    "cache_read_input_tokens":0,"cache_creation_input_tokens":0
                }
            }))
            .unwrap()
            .unwrap();

        assert_eq!(finish["choices"][0]["finish_reason"], "stop");
        assert_eq!(finish["usage"]["prompt_tokens"], 17);
        assert_eq!(finish["usage"]["completion_tokens"], 2);
        assert_eq!(finish["usage"]["prompt_tokens_details"]["cached_tokens"], 3);
    }
}
