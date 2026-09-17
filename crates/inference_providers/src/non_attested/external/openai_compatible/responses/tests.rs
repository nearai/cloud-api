use super::*;
use crate::non_attested::external::backend::ExternalBackend;
use bytes::Bytes;
use wiremock::{
    matchers::{body_partial_json, header, method, path},
    Mock, MockServer, ResponseTemplate,
};

const MODEL: &str = "gpt-6-astra";

fn params(value: Value) -> ChatCompletionParams {
    let mut params: ChatCompletionParams = serde_json::from_value(value.clone()).unwrap();
    params.original_request = Some(value);
    params
}

fn tool_params() -> ChatCompletionParams {
    params(json!({
        "model": MODEL, "messages": [{"role": "user", "content": "Get the weather"}],
        "tools": [{"type": "function", "function": {"name": "weather",
            "description": "Get weather", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}]
    }))
}

fn response(output: Value) -> Value {
    json!({
        "id": "resp_test", "created_at": 1234, "model": MODEL,
        "status": "completed", "service_tier": "priority", "output": output,
        "usage": {"input_tokens": 100, "output_tokens": 30, "total_tokens": 130,
            "input_tokens_details": {"cached_tokens": 40, "cache_write_tokens": 20},
            "output_tokens_details": {"reasoning_tokens": 25}}
    })
}

fn function_output() -> Value {
    json!([{"type": "reasoning", "id": "rs_test", "summary": []},
        {"type": "function_call", "id": "fc_test", "call_id": "call_weather",
         "name": "weather", "arguments": "{\"city\":\"Paris\"}"}])
}

#[test]
fn routing_is_scoped_to_astra_and_official_upstreams() {
    use super::super::uses_responses;
    for model in [MODEL, "gpt-6-astra-2026-09-01"] {
        assert!(uses_responses("https://api.openai.com/v1", model));
        assert!(uses_responses(
            "https://example.openai.azure.com/openai/v1",
            model
        ));
        assert!(!uses_responses(
            "https://api.openai.com.evil.test/v1",
            model
        ));
        assert!(!uses_responses("https://openrouter.ai/api/v1", model));
    }
    assert!(!uses_responses("https://api.openai.com/v1", "gpt-5.6-sol"));
    assert!(!uses_responses(
        "https://api.openai.com/v1",
        "gpt-6-astrafoo"
    ));
}

#[test]
fn request_preserves_tools_reasoning_limits_and_chat_schema_defaults() {
    let mut params = tool_params();
    params.max_tokens = Some(100);
    params.max_completion_tokens = Some(200);
    params.temperature = Some(0.5);
    params.top_p = Some(0.9);
    params.store = Some(true);
    params
        .extra
        .insert("reasoning_effort".into(), json!("high"));
    params.extra.insert("top_k".into(), json!(50));
    params.tool_choice = Some(
        serde_json::from_value(json!({"type": "function", "function": {"name": "weather"}}))
            .unwrap(),
    );
    params.parallel_tool_calls = Some(false);
    params.service_tier = Some(crate::ChatServiceTier::Priority);
    let body = build_request(&params, MODEL, true).unwrap();
    assert_eq!(
        body["input"],
        json!([{"role": "user", "content": "Get the weather"}])
    );
    assert_eq!(
        body["tools"][0],
        json!({"type": "function", "name": "weather", "description": "Get weather",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}, "strict": false})
    );
    assert_eq!(
        body["tool_choice"],
        json!({"type": "function", "name": "weather"})
    );
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["reasoning"], json!({"effort": "high"}));
    assert_eq!(body["max_output_tokens"], 200);
    assert_eq!(body["service_tier"], "priority");
    assert_eq!(body["store"], false);
    for field in [
        "temperature",
        "top_p",
        "top_k",
        "max_tokens",
        "max_completion_tokens",
        "reasoning_effort",
        "messages",
        "stream_options",
    ] {
        assert!(body.get(field).is_none(), "unexpected {field}");
    }
    params.original_request.as_mut().unwrap()["tools"][0]["function"]["strict"] = json!(true);
    assert_eq!(
        build_request(&params, MODEL, false).unwrap()["tools"][0]["strict"],
        true
    );
    for effort in ["none", "minimal"] {
        params
            .extra
            .insert("reasoning_effort".into(), json!(effort));
        assert_eq!(
            build_request(&params, MODEL, false).unwrap()["reasoning"]["effort"],
            "low"
        );
    }
}

#[test]
fn request_translates_multimodal_history_and_structured_output() {
    let params = params(json!({"model": MODEL, "messages": [
        {"role": "system", "content": "Be helpful"},
        {"role": "user", "content": [
            {"type": "text", "text": "Describe", "cache_control": {"type": "ephemeral"}},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA==", "detail": "low"}}]},
        {"role": "assistant", "content": [{"type": "text", "text": "A picture"}], "reasoning_content": "not a Responses input"}
    ], "response_format": {"type": "json_schema", "json_schema": {"name": "result", "schema": {"type": "object"}, "strict": true}}, "verbosity": "low"}));
    let body = build_request(&params, MODEL, false).unwrap();
    assert_eq!(
        body["input"][1]["content"],
        json!([
            {"type": "input_text", "text": "Describe"},
            {"type": "input_image", "image_url": "data:image/png;base64,AA==", "detail": "low"}
        ])
    );
    assert_eq!(
        body["input"][2]["content"],
        json!([{"type": "output_text", "text": "A picture", "annotations": []}])
    );
    assert_eq!(
        body["text"],
        json!({"format": {"type": "json_schema", "name": "result", "schema": {"type": "object"}, "strict": true}, "verbosity": "low"})
    );
}

#[test]
fn response_preserves_tool_ids_usage_and_tier() {
    let converted = convert_response(&response(function_output()), MODEL).unwrap();
    let call = &converted.choices[0].message.tool_calls.as_ref().unwrap()[0];
    assert_eq!(call.id.as_deref(), Some("call_weather")); // not fc_test
    assert_eq!(
        call.function.arguments.as_deref(),
        Some("{\"city\":\"Paris\"}")
    );
    assert_eq!(
        converted.choices[0].finish_reason.as_deref(),
        Some("tool_calls")
    );
    assert!(converted.choices[0].message.content.is_none());
    assert_eq!(converted.usage.completion_tokens, 30);
    assert_eq!(converted.usage.cached_tokens(), 40);
    assert_eq!(converted.usage.cache_write_tokens(), 20);
    assert_eq!(converted.service_tier.as_deref(), Some("priority"));
}

#[test]
fn incomplete_refused_and_failed_responses_are_not_successful_tool_calls() {
    let mut result = response(function_output());
    result["status"] = json!("incomplete");
    result["incomplete_details"] = json!({"reason": "max_output_tokens"});
    assert_eq!(
        convert_response(&result, MODEL).unwrap().choices[0]
            .finish_reason
            .as_deref(),
        Some("length")
    );
    result["incomplete_details"]["reason"] = json!("content_filter");
    assert_eq!(
        convert_response(&result, MODEL).unwrap().choices[0]
            .finish_reason
            .as_deref(),
        Some("content_filter")
    );
    let refused = response(
        json!([{"type": "message", "content": [{"type": "refusal", "refusal": "Cannot help"}]}]),
    );
    assert_eq!(
        convert_response(&refused, MODEL).unwrap().choices[0]
            .message
            .refusal
            .as_deref(),
        Some("Cannot help")
    );
    assert!(matches!(
        convert_response(
            &json!({"status": "failed", "error": {"code": "rate_limit_exceeded", "message": "Slow down"}}),
            MODEL
        ),
        Err(CompletionError::HttpError {
            status_code: 429,
            is_external: true,
            ..
        })
    ));
    result["usage"] = Value::Null;
    assert!(convert_response(&result, MODEL).is_err());
}

async fn mock_backend() -> (MockServer, OpenAiCompatibleBackend, BackendConfig) {
    let server = MockServer::start().await;
    // Exercise real host-based routing without sending any traffic to OpenAI.
    let backend = OpenAiCompatibleBackend {
        client: reqwest::Client::builder()
            .no_proxy()
            .resolve("api.openai.com", *server.address())
            .build()
            .unwrap(),
    };
    let config = BackendConfig {
        base_url: format!("http://api.openai.com:{}/v1", server.address().port()),
        api_key: "test-only".into(),
        ..Default::default()
    };
    (server, backend, config)
}

#[tokio::test]
async fn http_tool_call_result_and_final_answer_round_trip() {
    let (server, backend, config) = mock_backend().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("Authorization", "Bearer test-only"))
        .and(body_partial_json(
            json!({"stream": false, "input": [{"role": "user", "content": "Get the weather"}]}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(response(function_output())))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    let first = backend
        .chat_completion(&config, MODEL, tool_params())
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&first.raw_bytes).unwrap()["object"],
        "chat.completion"
    );
    let followup = params(json!({"model": MODEL, "messages": [
        {"role": "user", "content": "Get the weather"},
        {"role": "assistant", "tool_calls": first.response.choices[0].message.tool_calls},
        {"role": "tool", "tool_call_id": "call_weather", "content": "Sunny"}
    ]}));
    Mock::given(method("POST")).and(path("/v1/responses"))
        .and(body_partial_json(json!({"input": [
            {"role": "user", "content": "Get the weather"},
            {"type": "function_call", "call_id": "call_weather", "name": "weather", "arguments": "{\"city\":\"Paris\"}"},
            {"type": "function_call_output", "call_id": "call_weather", "output": "Sunny"}
        ]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(response(json!([
            {"type": "message", "content": [{"type": "output_text", "text": "It is sunny."}]}
        ])))).expect(1).mount(&server).await;
    let final_answer = backend
        .chat_completion(&config, MODEL, followup)
        .await
        .unwrap();
    assert_eq!(
        final_answer.response.choices[0].message.content.as_deref(),
        Some("It is sunny.")
    );
    assert_eq!(
        final_answer.response.choices[0].finish_reason.as_deref(),
        Some("stop")
    );
}

fn events() -> Vec<Value> {
    vec![
        json!({"type": "response.created", "response": {"id": "resp_test", "created_at": 1234, "service_tier": "default"}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning"}}),
        json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "function_call", "id": "fc_a", "call_id": "call_a", "name": "weather", "arguments": ""}}),
        json!({"type": "response.output_item.added", "output_index": 3, "item": {"type": "function_call", "id": "fc_b", "call_id": "call_b", "name": "weather", "arguments": ""}}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "{\"city\":"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 3, "delta": "{}"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "\"París\"}"}),
        json!({"type": "response.function_call_arguments.done", "output_index": 1, "arguments": "{\"city\":\"París\"}"}),
        json!({"type": "response.completed", "response": response(function_output())}),
    ]
}

fn wire(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| {
            format!(
                "event: {}\r\ndata: {}\r\n\r\n",
                e["type"].as_str().unwrap(),
                e
            )
        })
        .collect()
}

#[tokio::test]
async fn fragmented_stream_preserves_dense_indices_and_does_not_duplicate_arguments() {
    let wire = wire(&events());
    // Split even UTF-8 characters and SSE field names across network packets.
    let bytes: Vec<Result<Bytes, reqwest::Error>> = wire
        .as_bytes()
        .chunks(7)
        .map(|b| Ok(Bytes::copy_from_slice(b)))
        .collect();
    let mut stream = parse_stream(futures_util::stream::iter(bytes), MODEL.into());
    let mut arguments = [String::new(), String::new()];
    let mut ids = Vec::new();
    let mut last = None;
    while let Some(event) = stream.next().await {
        let event = event.unwrap();
        assert!(!event.raw_passthrough);
        let Some(StreamChunk::Chat(chunk)) = event.chunk else {
            panic!("expected chat chunk")
        };
        assert_eq!(chunk.id, "resp_test");
        for call in chunk.choices[0]
            .delta
            .as_ref()
            .unwrap()
            .tool_calls
            .iter()
            .flatten()
        {
            if let Some(id) = &call.id {
                ids.push(id.clone());
            }
            if let Some(args) = call.function.as_ref().and_then(|f| f.arguments.as_deref()) {
                arguments[call.index.unwrap() as usize].push_str(args);
            }
        }
        last = Some(chunk);
    }
    assert_eq!(ids, ["call_a", "call_b"]);
    assert_eq!(arguments, ["{\"city\":\"París\"}", "{}"]);
    let last = last.unwrap();
    assert_eq!(last.choices[0].finish_reason, Some(FinishReason::ToolCalls));
    assert_eq!(last.service_tier.as_deref(), Some("priority"));
    assert_eq!(last.usage.unwrap().cached_tokens(), 40);
}

#[tokio::test]
async fn http_stream_uses_responses_and_preserves_usage() {
    let (server, backend, config) = mock_backend().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(json!({"stream": true, "store": false})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(wire(&events()), "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let mut stream = backend
        .chat_completion_stream(&config, MODEL, tool_params())
        .await
        .unwrap();
    let mut terminal = false;
    while let Some(event) = stream.next().await {
        if let Some(StreamChunk::Chat(chunk)) = event.unwrap().chunk {
            if let Some(usage) = chunk.usage {
                assert_eq!(usage.completion_tokens, 30);
                terminal = true;
            }
        }
    }
    assert!(terminal);
}

#[tokio::test]
async fn errors_and_truncated_streams_surface_as_errors() {
    for events in [
        vec![
            json!({"type": "response.created", "response": {"id": "resp_test", "created_at": 1234}}),
        ],
        vec![
            json!({"type": "response.failed", "response": {"error": {"message": "upstream failed"}}}),
        ],
        vec![json!({"type": "error", "message": "upstream failed"})],
        vec![
            json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{}"}),
        ],
    ] {
        let bytes = futures_util::stream::iter(vec![Ok(Bytes::from(wire(&events)))]);
        let results: Vec<_> = parse_stream(bytes, MODEL.into()).collect().await;
        assert!(results.last().unwrap().is_err());
    }
    let (server, backend, config) = mock_backend().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(429).set_body_json(json!({"error": {"message": "Rate limited"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    assert!(matches!(
        backend.chat_completion(&config, MODEL, tool_params()).await,
        Err(CompletionError::HttpError {
            status_code: 429,
            is_external: true,
            ..
        })
    ));
}

#[test]
fn service_passthrough_fields_keep_token_limits_and_parallel_choice() {
    let mut params = tool_params();
    params.max_tokens = Some(50);
    params
        .extra
        .insert("max_completion_tokens".into(), json!(123));
    params
        .extra
        .insert("parallel_tool_calls".into(), json!(false));
    let body = build_request(&params, MODEL, false).unwrap();
    assert_eq!(body["max_output_tokens"], 123);
    assert_eq!(body["parallel_tool_calls"], false);
    params.n = Some(2);
    assert!(build_request(&params, MODEL, false).is_err());
    params.n = None;
    params
        .extra
        .insert("tools".into(), json!([{"type": "web_context_search"}]));
    assert!(build_request(&params, MODEL, false).is_err());
}

#[tokio::test]
async fn http_stream_tool_result_followup_returns_text_and_final_usage() {
    let (server, backend, config) = mock_backend().await;
    let followup = params(json!({"model": MODEL, "messages": [
        {"role": "user", "content": "Get the weather"},
        {"role": "assistant", "tool_calls": [{"id": "call_weather", "type": "function",
            "function": {"name": "weather", "arguments": "{}"}}]},
        {"role": "tool", "tool_call_id": "call_weather", "content": "Sunny"}
    ]}));
    let events = vec![
        json!({"type": "response.created", "response": {"id": "resp_final", "created_at": 1235}}),
        json!({"type": "response.output_text.delta", "delta": "It is "}),
        json!({"type": "response.output_text.delta", "delta": "sunny."}),
        json!({"type": "response.output_text.done", "text": "It is sunny."}),
        json!({"type": "response.completed", "response": response(json!([]))}),
    ];
    Mock::given(method("POST")).and(path("/v1/responses"))
        .and(body_partial_json(json!({"stream": true, "input": [
            {"role": "user", "content": "Get the weather"},
            {"type": "function_call", "call_id": "call_weather", "name": "weather", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_weather", "output": "Sunny"}
        ]})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(wire(&events), "text/event-stream"))
        .expect(1).mount(&server).await;
    let mut stream = backend
        .chat_completion_stream(&config, MODEL, followup)
        .await
        .unwrap();
    let mut text = String::new();
    let mut finished = false;
    while let Some(event) = stream.next().await {
        if let Some(StreamChunk::Chat(chunk)) = event.unwrap().chunk {
            if let Some(delta) = &chunk.choices[0].delta {
                text.push_str(delta.content.as_deref().unwrap_or(""));
            }
            if let Some(reason) = chunk.choices[0].finish_reason.as_ref() {
                assert_eq!(*reason, FinishReason::Stop);
                assert_eq!(chunk.usage.unwrap().total_tokens, 130);
                finished = true;
            }
        }
    }
    assert_eq!(text, "It is sunny.");
    assert!(finished);
}

#[tokio::test]
async fn stream_incomplete_and_refusal_events_keep_their_meaning() {
    for (reason, expected) in [
        ("max_output_tokens", FinishReason::Length),
        ("content_filter", FinishReason::ContentFilter),
    ] {
        let mut terminal = response(json!([]));
        terminal["status"] = json!("incomplete");
        terminal["incomplete_details"] = json!({"reason": reason});
        let events = vec![
            json!({"type": "response.created", "response": {"id": "resp_test", "created_at": 1234}}),
            json!({"type": "response.refusal.delta", "delta": "Cannot help"}),
            json!({"type": "response.incomplete", "response": terminal}),
        ];
        let bytes = futures_util::stream::iter(vec![Ok(Bytes::from(wire(&events)))]);
        let results: Vec<_> = parse_stream(bytes, MODEL.into()).collect().await;
        let Some(StreamChunk::Chat(ref refusal)) = results[1].as_ref().unwrap().chunk else {
            panic!("expected refusal")
        };
        assert_eq!(
            refusal.choices[0].delta.as_ref().unwrap().extra["refusal"],
            "Cannot help"
        );
        let Some(StreamChunk::Chat(ref last)) = results.last().unwrap().as_ref().unwrap().chunk
        else {
            panic!("expected terminal chunk")
        };
        assert_eq!(last.choices[0].finish_reason.as_ref(), Some(&expected));
    }
}
