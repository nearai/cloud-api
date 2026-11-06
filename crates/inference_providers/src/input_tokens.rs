//! Count the request that will execute, before acquiring a monetary dispatch claim.
//! Request content stays in the inference process; only the resulting count enters the ledger.
use crate::{ChatCompletionParams, CompletionError};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Chat,
    Completion,
    Responses,
    Messages,
    Embedding,
    Rerank,
    Score,
    Privacy,
    Decisions,
}

#[derive(Clone)]
pub struct InputTokenRequest {
    pub kind: InputKind,
    pub body: Value,
    pub extra: HashMap<String, Value>,
    pub chat: Option<Box<ChatCompletionParams>>,
    pub anthropic: Option<crate::AnthropicRawRequest>,
    // Internal protocol control, never deserialized from the caller's body.
    pub(crate) billing_probe: bool,
}

impl InputTokenRequest {
    pub fn chat(params: &ChatCompletionParams) -> Result<Self, CompletionError> {
        Ok(Self {
            kind: InputKind::Chat,
            body: serde_json::to_value(params).map_err(|_| unavailable())?,
            extra: params.extra.clone(),
            anthropic: None,
            billing_probe: false,
            chat: Some(Box::new(params.clone())),
        })
    }
    pub fn json(kind: InputKind, body: Value, extra: HashMap<String, Value>) -> Self {
        Self {
            kind,
            body,
            extra,
            chat: None,
            anthropic: None,
            billing_probe: false,
        }
    }
    /// These namespaced tools expand into multiple separately billed calls in the TEE.
    pub fn is_orchestrated(&self) -> bool {
        self.kind == InputKind::Chat
            && (self.body["tools"].as_array().is_some_and(|tools| {
                tools.iter().any(|tool| {
                    matches!(
                        tool["type"].as_str(),
                        Some("web_context_search" | "openrouter:fusion" | "nearai:fusion")
                    )
                })
            }) || self.body["plugins"].as_array().is_some_and(|plugins| {
                plugins
                    .iter()
                    .any(|p| p["id"] == "fusion" && p["enabled"].as_bool().unwrap_or(true))
            }))
    }
    pub fn charged_input(&self, count: i32) -> Result<i32, CompletionError> {
        let choices = if matches!(self.kind, InputKind::Chat | InputKind::Completion) {
            let n = self.body.get("n").and_then(Value::as_i64).unwrap_or(1);
            let best = self
                .body
                .get("best_of")
                .and_then(Value::as_i64)
                .unwrap_or(1);
            i32::try_from(n.max(best))
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(unavailable)?
        } else {
            1
        };
        count
            .checked_mul(choices)
            .filter(|n| *n >= 0)
            .ok_or_else(unavailable)
    }
}

pub fn unavailable() -> CompletionError {
    CompletionError::HttpError {
        status_code: 503,
        message: "Input token counting unavailable".into(),
        is_external: false,
    }
}

/// Read a small metadata response without reflecting provider errors or request content.
pub async fn read_count(response: reqwest::Response, field: &str) -> Result<i32, CompletionError> {
    let metadata = read_metadata_with_limit(response, 16 * 1024 * 1024).await?;
    metadata[field]
        .as_i64()
        .and_then(|n| i32::try_from(n).ok())
        .filter(|n| *n >= 0)
        .ok_or_else(unavailable)
}
pub async fn read_metadata(response: reqwest::Response) -> Result<Value, CompletionError> {
    read_metadata_with_limit(response, 16_384).await
}
async fn read_metadata_with_limit(
    response: reqwest::Response,
    limit: usize,
) -> Result<Value, CompletionError> {
    if !response.status().is_success() {
        return Err(unavailable());
    }
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(unavailable());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| unavailable())
}

/// Explicit fallback for text-only APIs without a counting endpoint. UTF-8 bytes are
/// an upper bound for byte/subword tokenizers; framing is charged per structured item.
/// Remote media and opaque/encrypted inputs require a native counter, never a size guess.
pub fn text_input_bound(body: &Value) -> Result<i32, CompletionError> {
    fn size(value: &Value) -> Option<usize> {
        match value {
            Value::String(s) => Some(s.len()),
            Value::Array(a) => a
                .iter()
                .try_fold(0usize, |n, v| n.checked_add(size(v)?.checked_add(32)?)),
            Value::Object(o) => o.iter().try_fold(32usize, |n, (k, v)| {
                n.checked_add(k.len())?.checked_add(size(v)?)
            }),
            _ => Some(16),
        }
    }
    fn media(value: &Value) -> bool {
        match value {
            Value::Array(a) => a.iter().any(media),
            Value::Object(o) => {
                o.keys().any(|k| {
                    matches!(
                        k.as_str(),
                        "image_url"
                            | "input_audio"
                            | "audio_url"
                            | "file_data"
                            | "file_url"
                            | "file_id"
                            | "data_base64"
                            | "inline_data"
                            | "inlineData"
                    )
                }) || o.values().any(media)
            }
            _ => false,
        }
    }
    // Do not interpret a function schema property named image_url as actual media.
    if ["messages", "input", "assets", "state", "questions"]
        .iter()
        .filter_map(|key| body.get(key))
        .any(media)
    {
        return Err(unavailable());
    }
    let fields = [
        "messages",
        "prompt",
        "input",
        "instructions",
        "system",
        "tools",
        "tool_choice",
        "response_format",
        "text",
        "query",
        "documents",
        "text_1",
        "text_2",
        "state",
        "questions",
        "assets",
    ];
    let mut total = 0usize;
    let mut found = false;
    for key in fields {
        if let Some(v) = body.get(key).filter(|v| !v.is_null()) {
            total = total
                .checked_add(size(v).ok_or_else(unavailable)?)
                .ok_or_else(unavailable)?;
            found = true;
        }
    }
    if let Some(docs) = body["documents"].as_array() {
        if let Some(query) = body.get("query") {
            total = total
                .checked_add(
                    size(query)
                        .ok_or_else(unavailable)?
                        .checked_mul(docs.len().saturating_sub(1))
                        .ok_or_else(unavailable)?,
                )
                .ok_or_else(unavailable)?;
        }
    }
    if !found {
        return Err(unavailable());
    }
    i32::try_from(total.checked_add(256).ok_or_else(unavailable)?).map_err(|_| unavailable())
}

/// Translate Chat's input structures to the Responses counting contract. Generation
/// parameters are excluded; input-affecting tools, schemas and reasoning are retained.
pub fn responses_count_body(request: &InputTokenRequest) -> Result<Value, CompletionError> {
    if request.kind == InputKind::Responses {
        let mut body = request.body.clone();
        body.as_object_mut()
            .ok_or_else(unavailable)?
            .retain(|key, _| {
                matches!(
                    key.as_str(),
                    "model"
                        | "input"
                        | "instructions"
                        | "tools"
                        | "tool_choice"
                        | "parallel_tool_calls"
                        | "text"
                        | "reasoning"
                        | "truncation"
                )
            });
        return Ok(body);
    }
    let mut input = Vec::new();
    for message in request.body["messages"]
        .as_array()
        .ok_or_else(unavailable)?
    {
        if message["role"] == "tool" {
            input.push(serde_json::json!({"type":"function_call_output", "call_id": message["tool_call_id"], "output":message["content"]}));
            continue;
        }
        if let Some(content) = message.get("content").filter(|v| !v.is_null()) {
            let mut content = content.clone();
            if let Some(parts) = content.as_array_mut() {
                for part in parts {
                    if part["type"] == "text" {
                        part["type"] = serde_json::json!(if message["role"] == "assistant" {
                            "output_text"
                        } else {
                            "input_text"
                        });
                    } else if part["type"] == "image_url" {
                        let image = part["image_url"].clone();
                        *part = serde_json::json!({"type":"input_image", "image_url":image["url"]});
                        if let Some(detail) = image.get("detail") {
                            part["detail"] = detail.clone();
                        }
                    }
                }
            }
            input.push(serde_json::json!({"role":message["role"], "content":content}));
        }
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                input.push(serde_json::json!({"type":"function_call", "call_id":call["id"], "name":call["function"]["name"], "arguments":call["function"]["arguments"]}));
            }
        }
    }
    let mut body = serde_json::json!({"model": request.body["model"], "input": input});
    if let Some(tools) = request.body.get("tools").and_then(Value::as_array) {
        body["tools"] = Value::Array(
            tools
                .iter()
                .map(|tool| {
                    if tool["type"] == "function" {
                        let mut function = tool["function"].clone();
                        function["type"] = serde_json::json!("function");
                        function
                    } else {
                        tool.clone()
                    }
                })
                .collect(),
        );
    }
    if let Some(format) = request.body.get("response_format").filter(|v| !v.is_null()) {
        let mut format = format.clone();
        if format["type"] == "json_schema" {
            format = format["json_schema"].clone();
            format["type"] = serde_json::json!("json_schema");
        }
        body["text"] = serde_json::json!({"format":format});
    }
    for key in ["tool_choice", "parallel_tool_calls"] {
        if let Some(value) = request.body.get(key).filter(|v| !v.is_null()) {
            body[key] = value.clone();
        }
    }
    if body["tool_choice"]["type"] == "function" {
        body["tool_choice"] =
            serde_json::json!({"type":"function", "name":body["tool_choice"]["function"]["name"]});
    }
    if let Some(effort) = request
        .body
        .get("reasoning_effort")
        .filter(|v| !v.is_null())
    {
        body["reasoning"] = serde_json::json!({"effort":effort});
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::non_attested::external::{
        backend::{BackendConfig, ExternalBackend},
        AnthropicBackend, GeminiBackend, OpenAiCompatibleBackend,
    };
    use serde_json::json;
    use wiremock::{
        matchers::{header, path, query_param},
        Mock, MockServer, ResponseTemplate,
    };

    #[test]
    fn fallback_tracks_the_request_and_rejects_opaque_media() {
        let short = json!({"messages":[{"role":"user","content":"hello"}]});
        let mut large = short.clone();
        large["messages"][0]["content"] = json!("中文".repeat(1000));
        assert!(text_input_bound(&short).unwrap() < 500);
        assert!(text_input_bound(&large).unwrap() >= 6000);
        assert!(text_input_bound(
            &json!({"input":[{"type":"input_image","image_url":"https://example/image"}]})
        )
        .is_err());
        assert!(text_input_bound(&json!({"tools":[{"type":"function","function":{"name":"f","parameters":{"properties":{"image_url":{"type":"string"}}}}}],"messages":[]})).is_ok());
        assert!(text_input_bound(&json!({"model":"no-input"})).is_err());
    }

    #[test]
    fn repeated_queries_and_hidden_choices_are_counted() {
        let one = text_input_bound(&json!({"query":"a".repeat(100),"documents":["one"]})).unwrap();
        let two =
            text_input_bound(&json!({"query":"a".repeat(100),"documents":["one","two"]})).unwrap();
        assert!(two - one >= 100);
        let request = InputTokenRequest::json(
            InputKind::Chat,
            json!({"n":2,"best_of":5}),
            Default::default(),
        );
        assert_eq!(request.charged_input(20).unwrap(), 100);
        assert!(request.charged_input(i32::MAX).is_err());
        assert!(request.charged_input(-1).is_err());
    }

    #[test]
    fn chat_count_preserves_tools_results_images_schemas_and_reasoning() {
        let request = InputTokenRequest::json(
            InputKind::Chat,
            json!({"model":"m","messages":[
            {"role":"system","content":"system"},
            {"role":"user","content":[{"type":"text","text":"hello"},{"type":"image_url","image_url":{"url":"data:image/png;base64,image","detail":"high"}}]},
            {"role":"assistant","tool_calls":[{"id":"call","type":"function","function":{"name":"f","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"call","content":"tool result"}
        ],"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}],
        "tool_choice":{"type":"function","function":{"name":"f"}},"parallel_tool_calls":false,"reasoning_effort":"high",
        "response_format":{"type":"json_schema","json_schema":{"name":"answer","schema":{"type":"object"}}},"max_tokens":99}),
            Default::default(),
        );
        let body = responses_count_body(&request).unwrap();
        assert_eq!(body["input"][0]["content"], "system");
        assert_eq!(body["input"][1]["content"][1]["detail"], "high");
        assert_eq!(body["input"][2]["type"], "function_call");
        assert_eq!(body["input"][3]["output"], "tool result");
        assert_eq!(body["tools"][0]["name"], "f");
        assert_eq!(body["tool_choice"]["name"], "f");
        assert_eq!(body["text"]["format"]["name"], "answer");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert!(body.get("max_tokens").is_none());
    }

    #[tokio::test]
    async fn native_messages_counter_preserves_version_beta_and_actual_input() {
        let server = MockServer::start().await;
        Mock::given(path("/v1/messages/count_tokens"))
            .and(header("anthropic-version", "2023-06-01"))
            .and(header("anthropic-beta", "count-fixture"))
            .and(query_param("beta", "true"))
            .respond_with(|r: &wiremock::Request| {
                let b: Value = serde_json::from_slice(&r.body).unwrap();
                assert_eq!(b["model"], "provider-model");
                assert_eq!(b["system"], "system prefix");
                assert_eq!(b["messages"][0]["content"], "hello");
                assert!(b.get("max_tokens").is_none());
                ResponseTemplate::new(200).set_body_json(json!({"input_tokens":21}))
            })
            .expect(1)
            .mount(&server)
            .await;
        let mut request = InputTokenRequest::json(
            InputKind::Messages,
            json!({"model":"catalog","system":"system prefix","messages":[{"role":"user","content":"hello"}],"max_tokens":100}),
            Default::default(),
        );
        request.anthropic = Some(crate::AnthropicRawRequest {
            endpoint: crate::AnthropicRawEndpoint::Messages,
            beta: true,
            body: request.body.clone(),
            headers: crate::AnthropicRawHeaders {
                version: Some("2023-06-01".into()),
                beta: Some("count-fixture".into()),
            },
        });
        let config = BackendConfig {
            base_url: format!("{}/v1", server.uri()),
            api_key: "fixture".into(),
            ..Default::default()
        };
        assert_eq!(
            AnthropicBackend::new()
                .count_input_tokens(&config, "provider-model", &request)
                .await
                .unwrap(),
            21
        );
    }

    #[tokio::test]
    async fn native_responses_counter_rejects_outages_and_invalid_counts() {
        for result in [
            json!({"input_tokens":12}),
            json!({"input_tokens":-1}),
            json!({"input_tokens":"12"}),
            json!({"input_tokens":2147483648_i64}),
            json!({"content":"sensitive marker"}),
        ] {
            let server = MockServer::start().await;
            Mock::given(path("/v1/responses/input_tokens"))
                .respond_with(ResponseTemplate::new(200).set_body_json(result.clone()))
                .expect(1)
                .mount(&server)
                .await;
            let config = BackendConfig {
                base_url: format!("{}/v1", server.uri()),
                ..Default::default()
            };
            let request = InputTokenRequest::json(
                InputKind::Responses,
                json!({"input":"hello"}),
                Default::default(),
            );
            let counted = OpenAiCompatibleBackend::new()
                .count_input_tokens(&config, "m", &request)
                .await;
            if result["input_tokens"] == 12 {
                assert_eq!(counted.unwrap(), 12);
            } else {
                assert!(!counted
                    .unwrap_err()
                    .to_string()
                    .contains("sensitive marker"));
            }
        }
        let server = MockServer::start().await;
        Mock::given(path("/tokenize"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let config = BackendConfig {
            base_url: format!("{}/v1", server.uri()),
            ..Default::default()
        };
        let request = InputTokenRequest::json(
            InputKind::Chat,
            json!({"messages":[{"role":"user","content":"hello"}]}),
            Default::default(),
        );
        assert!(OpenAiCompatibleBackend::new()
            .count_input_tokens(&config, "m", &request)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn gemini_counter_uses_the_same_converted_generate_request() {
        let server = MockServer::start().await;
        Mock::given(path("/v1beta/models/gemini-fixture:countTokens"))
            .respond_with(|r: &wiremock::Request| {
                let b: Value = serde_json::from_slice(&r.body).unwrap();
                assert_eq!(
                    b["generateContentRequest"]["contents"][0]["parts"][0]["text"],
                    "hello"
                );
                ResponseTemplate::new(200).set_body_json(json!({"totalTokens":9}))
            })
            .expect(1)
            .mount(&server)
            .await;
        let config = BackendConfig {
            base_url: format!("{}/v1beta", server.uri()),
            api_key: "fixture".into(),
            ..Default::default()
        };
        let params: ChatCompletionParams = serde_json::from_value(
            json!({"model":"m","messages":[{"role":"user","content":"hello"}],"max_tokens":10}),
        )
        .unwrap();
        assert_eq!(
            GeminiBackend::new()
                .count_input_tokens(
                    &config,
                    "google/gemini-fixture",
                    &InputTokenRequest::chat(&params).unwrap()
                )
                .await
                .unwrap(),
            9
        );
    }
}
