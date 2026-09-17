use super::*;

#[test]
fn selection_changes_only_explicit_stateless_allowlisted_models() {
    let models = vec!["openai/gpt-6-astra".into(), "custom/model".into()];
    let native = json!({"store":false});
    assert!(selected(&models, "openai/gpt-6-astra", &native));
    assert!(selected(&models, "custom/model", &native));
    assert!(!selected(&[], "openai/gpt-6-astra", &native));
    assert!(!selected(&models, "openai/gpt-6-astra-2026-09-03", &native));
    assert!(!selected(&models, "another/model", &native));
    for body in [
        json!({}),
        json!({"store":true}),
        json!({"store":null}),
        json!({"store":"false"}),
        json!({"store":false,"conversation":"conv_1"}),
        json!({"store":false,"previous_response_id":"resp_1"}),
        json!({"store":false,"background":true}),
    ] {
        assert!(!selected(&models, "openai/gpt-6-astra", &body));
    }
}

#[test]
fn native_validation_keeps_reasoning_and_function_schemas_but_rejects_builtins() {
    let mut body = json!({"model":"openai/gpt-6-astra", "store":false, "input":[
        {"type":"reasoning", "id":"rs_1", "encrypted_content":"opaque", "summary":[]},
        {"type":"function_call", "call_id":"call_1", "name":"lookup", "arguments":"{}"},
        {"type":"function_call_output", "call_id":"call_1", "output":"ok"}
    ], "tools":[{"type":"function", "name":"lookup", "strict":true}], "reasoning":{"effort":"high"}, "parallel_tool_calls":false});
    assert!(validate(&body).is_ok());
    body["tools"] = json!([{"type":"web_search"}]);
    assert!(validate(&body).is_err());
    body["tools"] = json!([]);
    body["service_tier"] = json!("priority");
    assert!(validate(&body).is_err());
}

#[test]
fn fragmented_native_events_keep_usage_including_cache_writes() {
    let wire = concat!(
        "event: response.created\r\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\r\n\r\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"París\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"service_tier\":\"default\",\"usage\":{\"input_tokens\":100,\"output_tokens\":30,\"total_tokens\":130,\"input_tokens_details\":{\"cached_tokens\":40,\"cache_write_tokens\":20}}}}\n\n"
    );
    let mut parser = EventUsage::default();
    for chunk in wire.as_bytes().chunks(3) {
        parser.push(chunk).unwrap();
    }
    parser.flush().unwrap();
    assert!(parser.usage.terminal);
    assert_eq!(parser.usage.id.as_deref(), Some("resp_1"));
    let usage = parser.usage.tokens.unwrap();
    assert_eq!(usage.completion_tokens, 30);
    assert_eq!(usage.cached_tokens(), 40);
    assert_eq!(usage.cache_write_tokens(), 20);
}

#[test]
fn incomplete_unknown_reason_keeps_billable_usage() {
    let mut usage = Usage::default();
    usage
        .apply(
            &json!({"status":"incomplete", "incomplete_details":{"reason":"future_reason"},
        "usage":{"input_tokens":5,"output_tokens":8,"total_tokens":13}}),
        )
        .unwrap();
    assert!(usage.terminal);
    assert!(matches!(usage.reason, Some(StopReason::Incomplete)));
    assert_eq!(usage.tokens.unwrap().total_tokens, 13);
}

#[test]
fn native_stateless_requests_cannot_reference_upstream_stored_resources() {
    for input in [
        json!([{"type":"item_reference","id":"msg_shared"}]),
        json!([{"type":"function_call_output","call_id":"call_1","output":[{"type":"input_image","file_id":"file_shared"}]}]),
        json!([{"role":"user","content":[{"type":"input_image","file_id":"file_shared"}]}]),
        json!([{"role":"user","content":[{"type":"input_file","file_id":"file_shared"}]}]),
    ] {
        assert!(validate(&json!({"input":input})).is_err());
    }
    assert!(validate(&json!({"prompt":{"id":"pmpt_shared"}})).is_err());
}

#[test]
fn usage_observer_does_not_treat_truncation_as_completion() {
    let mut parser = EventUsage::default();
    parser.push(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\n").unwrap();
    parser.flush().unwrap();
    assert!(!parser.usage.terminal);
    assert!(parser.usage.tokens.is_none());
    assert!(parser.push(b"data: {bad json}\n").is_err());
    let mut usage = Usage::default();
    assert!(usage.apply(&json!({"status":"completed","usage":{"input_tokens":-1,"output_tokens":1,"total_tokens":0}})).is_err());
}
