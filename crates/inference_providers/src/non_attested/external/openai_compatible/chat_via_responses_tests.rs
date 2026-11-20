use super::*;
use bytes::Bytes;
use futures_util::StreamExt;
use wiremock::{
    matchers::{body_partial_json, method, path},
    Mock, MockServer, ResponseTemplate,
};

fn params(value: Value) -> ChatCompletionParams {
    serde_json::from_value(value).unwrap()
}

fn tool_params() -> Value {
    json!({
        "model": "gpt-6.2-astra",
        "messages": [{"role": "user", "content": "weather?"}],
        "tools": [{"type": "function", "function": {
            "name": "weather", "description": "get weather",
            "parameters": {"type": "object", "properties": {}}}}],
    })
}

fn status_of(err: CompletionError) -> u16 {
    match err {
        CompletionError::HttpError { status_code, .. } => status_code,
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn astra_name_matching() {
    for ok in [
        "gpt-6-astra",
        "gpt-6.2-astra",
        "gpt-6.10-astra",
        "gpt-6.2-astra-2026-11-12",
        "gpt-6-astra-2026-11-12",
        "openai/gpt-6.2-astra",
    ] {
        assert!(is_gpt_6_astra(ok), "{ok}");
    }
    for bad in [
        "gpt-6.-astra",
        "gpt-60-astra",
        "gpt-6x-astra",
        "gpt-6-astral",
        "gpt-6-sol",
        "gpt-6.2-luna",
        "gpt-5.5-astra",
        "gpt-6.2-astra2",
    ] {
        assert!(!is_gpt_6_astra(bad), "{bad}");
    }
}

#[test]
fn routing_predicate() {
    let with_tools = params(tool_params());
    let openai = "https://api.openai.com/v1";
    assert!(routes_via_responses(
        &with_tools,
        openai,
        "gpt-6.2-astra",
        None
    ));
    assert!(routes_via_responses(
        &with_tools,
        "https://res.openai.azure.com/openai/v1",
        "my-deployment",
        Some("openai/gpt-6.2-astra")
    ));
    assert!(!routes_via_responses(
        &with_tools,
        openai,
        "gpt-6.2-sol",
        None
    ));
    assert!(!routes_via_responses(
        &with_tools,
        "https://api.together.xyz/v1",
        "gpt-6.2-astra",
        None
    ));
    let mut no_tools = tool_params();
    no_tools.as_object_mut().unwrap().remove("tools");
    assert!(!routes_via_responses(
        &params(no_tools.clone()),
        openai,
        "gpt-6.2-astra",
        None
    ));
    no_tools["tools"] = json!([]);
    assert!(!routes_via_responses(
        &params(no_tools),
        openai,
        "gpt-6.2-astra",
        None
    ));
}

#[test]
fn translates_conversation_and_tools() {
    let mut value = tool_params();
    value["messages"] = json!([
        {"role": "system", "content": "be brief"},
        {"role": "user", "content": [
            {"type": "text", "text": "look", "cache_control": {"type": "ephemeral"}},
            {"type": "image_url", "image_url": {"url": "https://x/y.png"}},
            {"type": "image_url", "image_url": {"url": "https://x/z.png", "detail": "high"}}
        ]},
        {"role": "assistant", "content": "checking", "tool_calls": [
            {"id": "call_1", "type": "function", "function": {"name": "weather", "arguments": "{\"c\":1}"}}
        ]},
        {"role": "tool", "tool_call_id": "call_1", "content": "sunny"}
    ]);
    value["tool_choice"] = json!("required");
    value["parallel_tool_calls"] = json!(false);
    value["max_tokens"] = json!(50);
    value["reasoning_effort"] = json!("high");
    value["response_format"] = json!({"type": "json_schema", "json_schema": {
        "name": "out", "schema": {"type": "object"}, "strict": true}});
    value["frequency_penalty"] = json!(0.5);
    value["top_k"] = json!(5);
    value["seed"] = json!(3);
    value["future_knob"] = json!({"a": 1});
    let p = params(value);
    let body = build_request(&p, true).unwrap();
    assert_eq!(
        body["input"],
        json!([
            {"role": "system", "content": "be brief"},
            {"role": "user", "content": [
                {"type": "input_text", "text": "look"},
                {"type": "input_image", "image_url": "https://x/y.png", "detail": "auto"},
                {"type": "input_image", "image_url": "https://x/z.png", "detail": "high"}
            ]},
            {"role": "assistant", "content": "checking"},
            {"type": "function_call", "call_id": "call_1", "name": "weather", "arguments": "{\"c\":1}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "sunny"}
        ])
    );
    assert_eq!(
        body["tools"],
        json!([{"type": "function", "name": "weather", "description": "get weather",
            "parameters": {"type": "object", "properties": {}}, "strict": false}])
    );
    assert_eq!(body["tool_choice"], "required");
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["max_output_tokens"], 50);
    assert_eq!(body["reasoning"], json!({"effort": "high"}));
    assert_eq!(
        body["text"]["format"],
        json!({"type": "json_schema", "name": "out", "schema": {"type": "object"}, "strict": true})
    );
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(body["future_knob"], json!({"a": 1}));
    for key in [
        "frequency_penalty",
        "top_k",
        "seed",
        "reasoning_effort",
        "response_format",
        "max_tokens",
    ] {
        assert!(body.get(key).is_none(), "{key}");
    }
    assert_eq!(build_request(&p, false).unwrap()["stream"], false);
}

#[test]
fn strict_and_function_tool_choice() {
    let mut value = tool_params();
    value["tools"][0]["function"]["strict"] = json!(true);
    value["tool_choice"] = json!({"type": "function", "function": {"name": "weather"}});
    let mut p = params(value.clone());
    p.original_request = Some(value);
    let body = build_request(&p, false).unwrap();
    assert_eq!(body["tools"][0]["strict"], true);
    assert_eq!(
        body["tool_choice"],
        json!({"type": "function", "name": "weather"})
    );
}

#[test]
fn rejects_unsupported_inputs() {
    let cases = [
        ("stop", json!(["x"])),
        ("n", json!(2)),
        ("logprobs", json!(true)),
        ("modalities", json!(["text", "audio"])),
    ];
    for (key, val) in cases {
        let mut v = tool_params();
        v[key] = val;
        assert_eq!(
            status_of(build_request(&params(v), false).unwrap_err()),
            400,
            "{key}"
        );
    }
    let mut v = tool_params();
    v["messages"] = json!([{"role": "user", "content": [
        {"type": "input_audio", "input_audio": {"data": "AAAA", "format": "wav"}}]}]);
    assert_eq!(
        status_of(build_request(&params(v), false).unwrap_err()),
        400
    );
    let mut v = tool_params();
    v["tools"][0]["type"] = json!("retrieval");
    assert_eq!(
        status_of(build_request(&params(v), false).unwrap_err()),
        400
    );
    let mut v = tool_params();
    v["messages"] = json!([{"role": "tool", "content": "x"}]);
    assert_eq!(
        status_of(build_request(&params(v), false).unwrap_err()),
        400
    );
    for (key, val) in [
        ("previous_response_id", json!("resp_1")),
        ("conversation", json!("conv_1")),
        ("background", json!(true)),
    ] {
        let mut v = tool_params();
        v[key] = val;
        assert_eq!(
            status_of(build_request(&params(v), false).unwrap_err()),
            400,
            "{key}"
        );
    }
    let mut ok = tool_params();
    ok["n"] = json!(1);
    assert!(build_request(&params(ok), false).is_ok());
}

fn usage_json() -> Value {
    json!({"input_tokens": 12, "output_tokens": 7, "total_tokens": 19,
        "input_tokens_details": {"cached_tokens": 4},
        "output_tokens_details": {"reasoning_tokens": 3}})
}

#[test]
fn converts_text_response() {
    let resp = json!({"id": "resp_1", "created_at": 1700, "model": "gpt-6.2-astra", "status": "completed",
        "service_tier": "default",
        "output": [{"type": "message", "content": [{"type": "output_text", "text": "Hel"},
            {"type": "output_text", "text": "lo"}]}],
        "usage": usage_json()});
    let out = convert_response(&resp).unwrap();
    assert_eq!(out.id, "resp_1");
    assert_eq!(out.created, 1700);
    assert_eq!(out.service_tier.as_deref(), Some("default"));
    let choice = &out.choices[0];
    assert_eq!(choice.message.content.as_deref(), Some("Hello"));
    assert!(choice.message.tool_calls.is_none());
    assert_eq!(choice.finish_reason.as_deref(), Some("stop"));
    assert_eq!(out.usage.prompt_tokens, 12);
    assert_eq!(out.usage.completion_tokens, 7);
    assert_eq!(out.usage.total_tokens, 19);
    assert_eq!(out.usage.cached_tokens(), 4);
    assert_eq!(
        out.usage
            .completion_tokens_details
            .as_ref()
            .unwrap()
            .reasoning_tokens,
        Some(3)
    );
}

#[test]
fn converts_tool_call_response_and_error_cases() {
    let resp = json!({"id": "resp_2", "status": "completed",
        "output": [{"type": "reasoning", "id": "rs_1", "summary": []},
            {"type": "function_call", "call_id": "call_9", "name": "weather", "arguments": "{\"a\":1}"}],
        "usage": usage_json()});
    let out = convert_response(&resp).unwrap();
    let choice = &out.choices[0];
    assert_eq!(choice.finish_reason.as_deref(), Some("tool_calls"));
    assert!(choice.message.content.is_none());
    let calls = choice.message.tool_calls.as_ref().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id.as_deref(), Some("call_9"));
    assert_eq!(calls[0].function.arguments.as_deref(), Some("{\"a\":1}"));

    let incomplete = json!({"id": "r", "status": "incomplete",
        "incomplete_details": {"reason": "max_output_tokens"},
        "output": [], "usage": usage_json()});
    assert_eq!(
        convert_response(&incomplete).unwrap().choices[0]
            .finish_reason
            .as_deref(),
        Some("length")
    );

    let failed = json!({"id": "r", "status": "failed", "error": {"message": "boom"}});
    assert_eq!(status_of(convert_response(&failed).unwrap_err()), 502);

    let no_usage = json!({"id": "r", "status": "completed", "output": []});
    assert!(convert_response(&no_usage).is_err());
    let bad_usage = json!({"id": "r", "status": "completed", "output": [],
        "usage": {"input_tokens": -1, "output_tokens": 1}});
    assert!(convert_response(&bad_usage).is_err());
}

fn sse(events: &[(&str, Value)]) -> String {
    events
        .iter()
        .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
        .collect()
}

async fn run_parser(text: String) -> Vec<Result<ChatCompletionChunkView, CompletionError>> {
    // Split mid-line to exercise buffering.
    let mid = text.len() / 2;
    let (a, b) = text.as_bytes().split_at(mid);
    let stream = futures_util::stream::iter(vec![
        Ok::<_, CompletionError>(Bytes::copy_from_slice(a)),
        Ok(Bytes::copy_from_slice(b)),
    ]);
    let parser: ResponsesChatSseParser<_> =
        BufferedSSEParser::new(stream, ResponsesChatState::default());
    parser
        .map(|r| {
            r.map(|e| match e.chunk {
                Some(StreamChunk::Chat(c)) => view(&c),
                other => panic!("unexpected chunk: {other:?}"),
            })
        })
        .collect()
        .await
}

type ChatCompletionChunkView = String;

fn view(chunk: &crate::ChatCompletionChunk) -> String {
    let choice = &chunk.choices[0];
    let delta = choice.delta.as_ref().unwrap();
    if let Some(reason) = &choice.finish_reason {
        let usage = chunk.usage.as_ref().unwrap();
        return format!(
            "finish({},{},{})",
            finish_reason_str(reason),
            usage.prompt_tokens,
            usage.completion_tokens
        );
    }
    if delta.role.is_some() {
        return "role".to_string();
    }
    if let Some(text) = &delta.content {
        return format!("text({text})");
    }
    if let Some(call) = delta.tool_calls.as_ref().and_then(|c| c.first()) {
        let f = call.function.as_ref().unwrap();
        return match (&call.id, &f.name, &f.arguments) {
            (Some(id), Some(name), _) => format!("start({},{id},{name})", call.index.unwrap()),
            (_, _, Some(args)) => format!("args({},{args})", call.index.unwrap()),
            _ => "tool?".to_string(),
        };
    }
    "other".to_string()
}

#[tokio::test]
async fn streams_tool_calls() {
    let created = json!({"type": "response.created", "response": {"id": "resp_s", "model": "gpt-6.2-astra", "created_at": 5}});
    let text = sse(&[
        ("response.created", created),
        (
            "response.in_progress",
            json!({"type": "response.in_progress"}),
        ),
        (
            "response.output_item.added",
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs"}}),
        ),
        (
            "response.output_item.added",
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "function_call", "call_id": "c1", "name": "weather", "arguments": ""}}),
        ),
        (
            "response.function_call_arguments.delta",
            json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "{\"a\""}),
        ),
        (
            "response.function_call_arguments.delta",
            json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": ":1}"}),
        ),
        (
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": 1}),
        ),
        (
            "response.output_item.added",
            json!({"type": "response.output_item.added", "output_index": 2, "item": {"type": "function_call", "call_id": "c2", "name": "time", "arguments": ""}}),
        ),
        (
            "response.function_call_arguments.delta",
            json!({"type": "response.function_call_arguments.delta", "output_index": 2, "delta": "{}"}),
        ),
        (
            "response.completed",
            json!({"type": "response.completed", "response": {"status": "completed", "usage": usage_json()}}),
        ),
    ]);
    let got: Vec<String> = run_parser(text)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        got,
        vec![
            "role",
            "start(0,c1,weather)",
            "args(0,{\"a\")",
            "args(0,:1})",
            "start(1,c2,time)",
            "args(1,{})",
            "finish(tool_calls,12,7)",
        ]
    );
}

#[tokio::test]
async fn streams_text_and_failures() {
    let created =
        json!({"type": "response.created", "response": {"id": "r", "model": "m", "created_at": 1}});
    let text = sse(&[
        ("response.created", created.clone()),
        (
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "delta": "Hi"}),
        ),
        (
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "delta": " there"}),
        ),
        (
            "response.completed",
            json!({"type": "response.completed", "response": {"status": "completed", "usage": usage_json()}}),
        ),
    ]);
    let got: Vec<String> = run_parser(text)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        got,
        vec!["role", "text(Hi)", "text( there)", "finish(stop,12,7)"]
    );

    let failed = sse(&[
        ("response.created", created),
        (
            "response.failed",
            json!({"type": "response.failed", "response": {"error": {"message": "nope"}}}),
        ),
    ]);
    let got = run_parser(failed).await;
    assert_eq!(got.len(), 2);
    assert!(got[0].is_ok());
    assert_eq!(status_of(got[1].clone().unwrap_err()), 502);

    let early = sse(&[(
        "response.output_text.delta",
        json!({"type": "response.output_text.delta", "delta": "x"}),
    )]);
    assert!(run_parser(early).await[0].is_err());
}

// ---- end to end against a mock upstream ----

/// Backend whose "api.openai.com" host resolves to the mock server, so the
/// OpenAI-source check passes.
async fn openai_backend() -> (MockServer, OpenAiCompatibleBackend, BackendConfig) {
    let server = MockServer::start().await;
    let backend = OpenAiCompatibleBackend {
        client: reqwest::Client::builder()
            .no_proxy()
            .resolve("api.openai.com", *server.address())
            .build()
            .unwrap(),
    };
    let config = BackendConfig {
        base_url: format!("http://api.openai.com:{}/v1", server.address().port()),
        api_key: "test".into(),
        timeout_seconds: 30,
        ..Default::default()
    };
    (server, backend, config)
}

fn response_json() -> Value {
    json!({"id": "resp_e2e", "created_at": 9, "model": "gpt-6.2-astra", "status": "completed",
        "output": [{"type": "function_call", "call_id": "call_e", "name": "weather", "arguments": "{}"}],
        "usage": usage_json()})
}

#[tokio::test]
async fn backend_routes_astra_tools_to_responses() {
    let (server, backend, config) = openai_backend().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(json!({
            "model": "gpt-6.2-astra", "store": false, "stream": false,
            "tools": [{"type": "function", "name": "weather", "strict": false}]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(response_json()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let out = backend
        .chat_completion(&config, "gpt-6.2-astra", params(tool_params()))
        .await
        .unwrap();
    let calls = out.response.choices[0].message.tool_calls.as_ref().unwrap();
    assert_eq!(calls[0].function.name.as_deref(), Some("weather"));
    assert_eq!(
        out.response.choices[0].finish_reason.as_deref(),
        Some("tool_calls")
    );
}

#[tokio::test]
async fn backend_streams_astra_tools_via_responses() {
    let (server, backend, config) = openai_backend().await;
    let body = sse(&[
        (
            "response.created",
            json!({"type": "response.created", "response": {"id": "r", "model": "m", "created_at": 1}}),
        ),
        (
            "response.output_item.added",
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "function_call", "call_id": "c", "name": "weather"}}),
        ),
        (
            "response.function_call_arguments.delta",
            json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{}"}),
        ),
        (
            "response.completed",
            json!({"type": "response.completed", "response": {"usage": usage_json()}}),
        ),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(json!({"store": false, "stream": true})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let stream = backend
        .chat_completion_stream(&config, "gpt-6.2-astra", params(tool_params()))
        .await
        .unwrap();
    let items: Vec<_> = stream.collect().await;
    assert_eq!(items.len(), 4);
    assert!(items.iter().all(Result::is_ok));
}

#[tokio::test]
async fn backend_keeps_non_astra_tools_on_chat_completions() {
    let (server, backend, config) = openai_backend().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {"message": "x"}})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let mut value = tool_params();
    value["model"] = json!("gpt-6.2-sol");
    assert!(backend
        .chat_completion(&config, "gpt-6.2-sol", params(value))
        .await
        .is_err());
}
