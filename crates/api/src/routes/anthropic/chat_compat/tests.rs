use super::*;
use futures_util::{stream as futures_stream, StreamExt};
use inference_providers::{models::ChatCompletionChunk, SSEEvent, StreamChunk};

fn request(content: Value) -> Value {
    json!({"model":"private-chat","max_tokens":100,"messages":content})
}

#[test]
fn translates_claude_tool_history_and_images() {
    let mut body = request(json!([
        {"role":"user","content":"Open the file"},
        {"role":"assistant","content":[{"type":"thinking","thinking":"Use the tool","signature":""},
            {"type":"tool_use","id":"call-a","name":"Read","input":{"file_path":"sample.txt"}},
            {"type":"tool_use","id":"call-b","name":"Read","input":{"file_path":"second.txt"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-a","content":[
            {"type":"text","text":"image from file"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"YQ=="}}]},
            {"type":"tool_result","tool_use_id":"call-b","is_error":true,"content":"missing"},
            {"type":"text","text":"summarize"}]}
    ]));
    body["tools"] = json!([{"name":"Read","input_schema":{"type":"object","properties":{"file_path":{"type":"string"}}}}]);
    body["tool_choice"] = json!({"type":"tool","name":"Read","disable_parallel_tool_use":true});
    body["system"] = json!([{"type":"text","text":"first","cache_control":{"type":"ephemeral"}},{"type":"text","text":"second"}]);
    body["output_config"] = json!({"effort":"high"});
    body["stream"] = json!(true);
    let chat = request::translate(&body, "vendor/private", true).unwrap();
    assert_eq!(chat.model, "vendor/private");
    let chat = serde_json::to_value(chat).unwrap();
    let m = &chat["messages"];
    assert_eq!(m[0]["content"], "first\n\nsecond");
    assert_eq!(m[2]["reasoning_content"], "Use the tool");
    assert_eq!(
        m[2]["tool_calls"][0]["function"]["arguments"],
        r#"{"file_path":"sample.txt"}"#
    );
    assert_eq!(m[3]["tool_call_id"], "call-a");
    assert_eq!(m[4]["tool_call_id"], "call-b");
    assert_eq!(m[4]["content"], "Tool error:\nmissing");
    assert_eq!(
        m[5]["content"][0]["image_url"]["url"],
        "data:image/png;base64,YQ=="
    );
    assert_eq!(chat["tool_choice"]["function"]["name"], "Read");
    assert_eq!(chat["parallel_tool_calls"], false);
    assert_eq!(chat["stream_options"]["include_usage"], true);
    assert_eq!(chat["reasoning_effort"], "high");
}

#[test]
fn cannot_translate_redacted_or_unknown_content_as_plain_text() {
    for block in [
        json!({"type":"redacted_thinking","data":"opaque"}),
        json!({"type":"server_tool_use","name":"fetch"}),
    ] {
        let err = request::translate(
            &request(json!([{"role":"assistant","content":[block]}])),
            "chat",
            true,
        )
        .unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }
}

#[test]
fn translates_json_with_cache_counts_and_tool_replay() {
    let mut chat:ChatCompletionResponse=serde_json::from_value(json!({"id":"chat-123","object":"chat.completion","model":"upstream","created":1,
        "choices":[{"index":0,"message":{"role":"assistant","reasoning_content":"think","content":"text",
            "tool_calls":[{"id":"call-id","type":"function","function":{"name":"lookup","arguments":"{\"q\":1}"}}]},"finish_reason":"tool_calls"}],
        "usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":5,"cache_write_tokens":2}}})).unwrap();
    let msg = from_chat(&chat, "vendor/private", true).unwrap();
    assert_eq!(msg["id"], "chat-123");
    assert_eq!(msg["stop_reason"], "tool_use");
    assert_eq!(
        msg["content"][0],
        json!({"type":"thinking","thinking":"think","signature":""})
    );
    assert_eq!(
        msg["content"][2],
        json!({"type":"tool_use","id":"call-id","name":"lookup","input":{"q":1}})
    );
    assert_eq!(
        msg["usage"],
        json!({"input_tokens":5,"output_tokens":3,"cache_read_input_tokens":5,"cache_creation_input_tokens":2})
    );
    let hidden = from_chat(&chat, "vendor/private", false).unwrap();
    assert_eq!(hidden["content"][0]["type"], "text");
    for (finish, expected) in [
        ("length", "max_tokens"),
        ("content_filter", "refusal"),
        ("stop", "tool_use"),
    ] {
        chat.choices[0].finish_reason = Some(finish.to_owned());
        let msg = from_chat(&chat, "vendor/private", false).unwrap();
        assert_eq!(
            msg["stop_reason"], expected,
            "upstream finish_reason={finish}"
        );
    }
    chat.choices[0].finish_reason = Some("stop".into());
    let call = &mut chat.choices[0].message.tool_calls.as_mut().unwrap()[0];
    call.function.arguments = Some("{".into());
    assert!(from_chat(&chat, "vendor/private", false).is_err());
    chat.choices[0].message.tool_calls = None;
    assert_eq!(
        from_chat(&chat, "vendor/private", false).unwrap()["stop_reason"],
        "end_turn"
    );
}

fn chunk(choices: Value, usage: Value) -> SSEEvent {
    let chunk:ChatCompletionChunk=serde_json::from_value(json!({"id":"chat-123","model":"upstream","object":"chat.completion.chunk","created":1,"choices":choices,"usage":usage})).unwrap();
    SSEEvent {
        raw_bytes: Default::default(),
        chunk: Some(StreamChunk::Chat(chunk)),
        raw_passthrough: false,
    }
}
fn delta(d: Value, finish: Option<&str>) -> SSEEvent {
    chunk(
        json!([{"index":0,"delta":d,"finish_reason":finish}]),
        Value::Null,
    )
}
async fn events(chunks: Vec<SSEEvent>) -> Vec<Value> {
    let source = Box::pin(futures_stream::iter(chunks.into_iter().map(Ok)));
    let (body, id) = stream::convert(source, "private".into(), true)
        .await
        .unwrap();
    assert_eq!(id.as_deref(), Some("chat-123"));
    body.map(|bytes| {
        let bytes = bytes.unwrap();
        let data = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap();
        serde_json::from_str(data).unwrap()
    })
    .collect()
    .await
}

#[tokio::test]
async fn interleaved_tool_deltas_form_contiguous_blocks_and_late_usage_survives() {
    let first =
        json!({"index":0,"id":"one","function":{"name":"Read","arguments":"{\"file_path\":"}});
    let second = json!({"index":1,"id":"two","function":{"name":"List","arguments":"{"}});
    let chunks = vec![
        delta(json!({"reasoning":"consider"}), None),
        delta(json!({"content":"look"}), None),
        delta(json!({"tool_calls":[first,second]}), None),
        delta(
            json!({"tool_calls":[{"index":1,"function":{"arguments":"}"}},{"index":0,"function":{"arguments":"\"a\"}"}}]}),
            Some("tool_calls"),
        ),
        chunk(
            json!([]),
            json!({"prompt_tokens":9,"completion_tokens":4,"total_tokens":13}),
        ),
    ];
    let output = events(chunks).await;
    assert_eq!(output.first().unwrap()["message"]["id"], "chat-123");
    let starts: Vec<_> = output
        .iter()
        .filter(|e| e["type"] == "content_block_start")
        .map(|e| e["content_block"]["type"].as_str().unwrap())
        .collect();
    assert_eq!(starts, ["thinking", "text", "tool_use", "tool_use"]);
    for (index, args) in [(2, json!({"file_path":"a"})), (3, json!({}))] {
        let raw: String = output
            .iter()
            .filter(|e| e["index"] == index && e["delta"]["type"] == "input_json_delta")
            .filter_map(|e| e["delta"]["partial_json"].as_str())
            .collect();
        assert_eq!(serde_json::from_str::<Value>(&raw).unwrap(), args);
    }
    let terminal = output
        .iter()
        .find(|e| e["type"] == "message_delta")
        .unwrap();
    assert_eq!(terminal["delta"]["stop_reason"], "tool_use");
    assert_eq!(terminal["usage"]["input_tokens"], 9);
    assert_eq!(terminal["usage"]["output_tokens"], 4);
    assert_eq!(output.last().unwrap()["type"], "message_stop");
}

#[tokio::test]
async fn streamed_stop_reason_accounts_for_a_completed_tool_call() {
    for (finish, expected) in [
        ("length", "max_tokens"),
        ("content_filter", "refusal"),
        ("stop", "tool_use"),
    ] {
        let output = events(vec![
            delta(json!({"tool_calls":[{"index":0,"id":"call-1","function":{"name":"lookup","arguments":"{"}}]}), None),
            delta(json!({"tool_calls":[{"index":0,"function":{"arguments":"}"}}]}), None),
            delta(json!({}), Some(finish)),
        ]).await;
        assert_eq!(output.last().unwrap()["type"], "message_stop");
        let terminal = output
            .iter()
            .find(|e| e["type"] == "message_delta")
            .unwrap();
        assert_eq!(
            terminal["delta"]["stop_reason"], expected,
            "upstream finish_reason={finish}"
        );
    }
}

#[tokio::test]
async fn incomplete_stream_or_bad_tool_json_never_emits_success() {
    for chunks in [
        vec![delta(json!({"content":"partial"}), None)],
        vec![delta(
            json!({"tool_calls":[{"index":0,"id":"one","function":{"name":"Read","arguments":"{"}}]}),
            Some("tool_calls"),
        )],
        vec![delta(
            json!({"tool_calls":[{"index":0,"id":"one","function":{"name":"Read","arguments":"{"}}]}),
            Some("stop"),
        )],
    ] {
        let out = events(chunks).await;
        assert_eq!(out.last().unwrap()["type"], "error");
        assert!(!out
            .iter()
            .any(|e| matches!(e["type"].as_str(), Some("message_delta" | "message_stop"))));
    }
}

#[test]
fn cli_reasoning_hints_follow_the_model_and_thinking_setting() {
    let mut body = request(json!([{"role":"user","content":"hi"}]));
    body["output_config"] = json!({"effort":"high"});
    body["thinking"] = json!({"type":"adaptive","display":"omitted"});
    body["context_management"] = json!({"edits":[{"type":"clear_thinking_20251015","keep":"all"}]});
    let unsupported = request::translate(&body, "chat", false).unwrap();
    assert!(!unsupported.extra.contains_key("reasoning_effort"));
    body["thinking"] = json!({"type":"disabled"});
    assert_eq!(
        request::translate(&body, "chat", true).unwrap().extra["reasoning_effort"],
        "none"
    );
    body.as_object_mut().unwrap().remove("output_config");
    body["thinking"] = json!({"type":"enabled","budget_tokens":8192});
    assert_eq!(
        request::translate(&body, "chat", true).unwrap().extra["reasoning_effort"],
        "medium"
    );
}

#[tokio::test]
async fn a_text_suffix_can_follow_large_streamed_tool_arguments() {
    let argument = "中文".repeat(10_000);
    let raw = json!({"text":argument}).to_string();
    let out = events(vec![
        delta(json!({"tool_calls":[{"index":0,"id":"one","function":{"name":"write","arguments":raw}}]}),None),
        delta(json!({"content":"after the tool"}), Some("tool_calls")),
    ]).await;
    let received: String = out
        .iter()
        .filter_map(|e| e["delta"]["partial_json"].as_str())
        .collect();
    assert_eq!(received, raw);
    assert!(out
        .iter()
        .filter_map(|e| e["delta"]["partial_json"].as_str())
        .all(|part| part.len() <= 32 * 1024));
    assert!(out
        .iter()
        .any(|e| e["index"] == 1 && e["delta"]["text"] == "after the tool"));
    assert_eq!(out.last().unwrap()["type"], "message_stop");
}
