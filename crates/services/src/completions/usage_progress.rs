//! Request-local counts for interrupted streams; chunk counts are a lower bound.

use inference_providers::StreamChunk;

#[derive(Default)]
pub(super) struct UsageProgress {
    pub prompt_tokens: Option<i32>,
    pub completion_tokens: Option<i32>,
    pub partial: bool,
}

impl UsageProgress {
    pub fn observe(&mut self, chunk: &StreamChunk) {
        let usage = match chunk {
            StreamChunk::Chat(chat) => chat.usage.as_ref(),
            StreamChunk::Text(text) => text.usage.as_ref(),
        };
        if let Some(usage) = usage {
            if usage.prompt_tokens >= 0 {
                self.prompt_tokens = Some(usage.prompt_tokens);
            }
            if usage.completion_tokens >= 0 {
                self.completion_tokens = Some(usage.completion_tokens);
                self.partial = false;
                return;
            }
        }
        if chunk_has_content(chunk) {
            self.completion_tokens = Some(self.completion_tokens.unwrap_or(0).saturating_add(1));
            self.partial = true;
        }
    }
}

fn nonempty(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

fn chunk_has_content(chunk: &StreamChunk) -> bool {
    match chunk {
        StreamChunk::Text(text) => text.choices.iter().any(|choice| !choice.text.is_empty()),
        StreamChunk::Chat(chat) => chat.choices.iter().any(|choice| {
            choice.delta.as_ref().is_some_and(|delta| {
                nonempty(delta.content.as_deref())
                    || nonempty(delta.reasoning_content.as_deref())
                    || nonempty(delta.reasoning.as_deref())
                    || delta.tool_calls.as_ref().is_some_and(|calls| {
                        calls.iter().any(|call| {
                            call.function.as_ref().is_some_and(|function| {
                                nonempty(function.name.as_deref())
                                    || nonempty(function.arguments.as_deref())
                            })
                        })
                    })
                    || delta.extra.values().any(value_has_content)
            })
        }),
    }
}

/// Match content-bearing fields in a forwarded OpenAI SSE data payload.
pub fn payload_has_content(payload: &serde_json::Value) -> bool {
    payload
        .get("choices")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|choices| {
            choices.iter().any(|choice| {
                choice.get("text").is_some_and(value_has_content)
                    || choice
                        .get("delta")
                        .and_then(serde_json::Value::as_object)
                        .is_some_and(|delta| {
                            delta
                                .iter()
                                .any(|(key, value)| key != "role" && value_has_content(value))
                        })
            })
        })
}

fn value_has_content(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::String(value) => !value.is_empty(),
        serde_json::Value::Array(values) => values.iter().any(value_has_content),
        serde_json::Value::Object(values) => values.values().any(value_has_content),
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => true,
    }
}
