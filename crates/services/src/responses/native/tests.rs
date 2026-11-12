use super::*;

#[test]
fn selection_routes_every_canonical_openai_model_natively() {
    for model in [
        "openai/gpt-4.1",
        "openai/gpt-5.4",
        "openai/gpt-6.1-sol",
        "openai/gpt-6-astra",
        "openai/gpt-6-astra-2026-09-03",
        "openai/future-model",
    ] {
        assert!(selected(model), "{model}");
    }
    for model in [
        "custom/model",
        "anthropic/claude",
        "Qwen/model",
        "openai-compatible/model",
    ] {
        assert!(!selected(model), "{model}");
    }
}

#[test]
fn native_requests_default_to_explicit_no_store_and_preserve_provider_fields() {
    for mut body in [
        json!({}),
        json!({"store":null}),
        json!({"store":false}),
        json!({"store":false,"conversation":null,"previous_response_id":null,"background":null}),
    ] {
        body["reasoning"] = json!({"effort":"high"});
        body["include"] = json!(["reasoning.encrypted_content"]);
        let mut expected = body.clone();
        expected["store"] = json!(false);
        normalize_stateless_request(&mut body).unwrap();
        assert_eq!(body, expected);
        assert!(inference_providers::responses_raw::is_stateless(&body));
    }
}

#[test]
fn native_requests_reject_stateful_or_invalid_fields_before_dispatch() {
    for mut body in [
        json!(null),
        json!([]),
        json!({"store":true}),
        json!({"store":"false"}),
        json!({"store":false,"conversation":"conv_1"}),
        json!({"store":false,"previous_response_id":"resp_1"}),
        json!({"store":false,"background":true}),
    ] {
        assert!(normalize_stateless_request(&mut body).is_err(), "{body}");
    }
}

#[test]
fn native_validation_accepts_replay_and_rejects_unsupported_service_tiers() {
    let mut body = json!({"model":"openai/gpt-6-astra", "store":false, "input":[
        {"type":"reasoning", "id":"rs_1", "encrypted_content":"opaque", "summary":[]},
        {"type":"function_call", "call_id":"call_1", "name":"lookup", "arguments":"{}"},
        {"type":"function_call_output", "call_id":"call_1", "output":"ok"}
    ], "tools":[{"type":"function", "name":"lookup", "strict":true}], "reasoning":{"effort":"high"}, "parallel_tool_calls":false});
    assert!(validate(&body).is_ok());
    body["service_tier"] = json!("priority");
    assert!(validate(&body).is_err());
}

#[test]
fn native_web_search_preserves_configuration_choice_and_include() {
    for choice in [json!("required"), json!({"type":"web_search"})] {
        let body = json!({
            "model":"openai/gpt-6-astra", "store":false, "input":"Find current sources.",
            "tools":[
                {"type":"web_search", "search_context_size":"high", "external_web_access":false,
                    "filters":{"allowed_domains":["example.com"]},
                    "user_location":{"type":"approximate", "country":"US", "city":"Boston"}},
                {"type":"function", "name":"lookup", "strict":true},
                {"type":"namespace", "name":"client", "tools":[
                    {"type":"custom", "name":"apply_patch", "format":{"type":"text"}}
                ]}
            ],
            "tool_choice":choice,
            "include":["web_search_call.action.sources", "reasoning.encrypted_content", "web_search_call.results"],
            "provider_extension":{"preserve":"exactly"}
        });
        let normalized =
            crate::responses::client_tools::normalize_native_request(body.clone()).unwrap();
        assert!(validate(&normalized).is_ok());
        assert_eq!(normalized, body);
    }
}

#[test]
fn native_web_search_replays_complete_output_without_transforming_it() {
    let body = json!({
        "model":"openai/gpt-6-astra", "store":false,
        "tools":[{"type":"web_search"}], "tool_choice":"none",
        "include":["web_search_call.action.sources", "reasoning.encrypted_content"],
        "input":[
            {"role":"user", "content":"Find a source."},
            {"type":"reasoning", "id":"rs_old", "summary":[], "encrypted_content":"opaque_fixture"},
            {"type":"web_search_call", "id":"ws_old", "status":"completed", "action":{
                "type":"search", "query":"Synthetic query", "queries":["Synthetic query"],
                "sources":[{"type":"url", "url":"https://example.com/source"}]
            }},
            {"type":"message", "id":"msg_old", "role":"assistant", "status":"completed", "phase":"final_answer",
                "content":[{"type":"output_text", "text":"A sourced answer.", "annotations":[{
                    "type":"url_citation", "url":"https://example.com/source", "title":"Synthetic source", "start_index":2, "end_index":9
                }]}]},
            {"role":"user", "content":"Summarize that answer without another search."}
        ]
    });
    let normalized =
        crate::responses::client_tools::normalize_native_request(body.clone()).unwrap();
    assert!(validate(&normalized).is_ok());
    assert_eq!(normalized, body);
}

#[test]
fn native_client_tools_preserve_grammar_namespaces_and_custom_replay() {
    let body = json!({
        "model": "openai/gpt-6-astra", "store": false,
        "input": [
            {"type":"reasoning", "id":"rs_1", "encrypted_content":"opaque", "summary":[]},
            {"type":"custom_tool_call", "id":"ct_1", "call_id":"call_patch", "name":"apply_patch", "input":"*** Begin Patch\n*** End Patch"},
            {"type":"custom_tool_call_output", "call_id":"call_patch", "output":"Done"}
        ],
        "tools": [
            {"type":"custom", "name":"apply_patch", "format":{
                "type":"grammar", "syntax":"lark", "definition":"start: /.+/s"
            }},
            {"type":"namespace", "name":"functions", "description":"Local commands", "tools":[
                {"type":"function", "name":"exec_command", "strict":true, "parameters":{
                    "type":"object", "properties":{"cmd":{"type":"string"}},
                    "required":["cmd"], "additionalProperties":false
                }}
            ]}
        ],
        "include":["reasoning.encrypted_content"],
        "reasoning":{"effort":"high"},
        "parallel_tool_calls":false,
        "provider_extension":{"preserve":"exactly"}
    });
    let normalized =
        crate::responses::client_tools::normalize_native_request(body.clone()).unwrap();
    assert!(validate(&normalized).is_ok());
    assert_eq!(
        normalized, body,
        "native tools and replay stay in their original wire shape"
    );
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
        json!([{"type":"file_search_call","id":"fs_shared","status":"completed"}]),
        json!([{"type":"mcp_call","id":"mcp_shared","status":"completed"}]),
        json!([{"type":"function_call_output","call_id":"call_1","output":[{"type":"input_image","file_id":"file_shared"}]}]),
        json!([{"type":"custom_tool_call_output","call_id":"call_patch","output":[{"type":"input_file","file_id":"file_shared"}]}]),
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

#[tokio::test]
async fn interrupted_stream_bills_observed_usage_against_its_reservation() {
    use crate::test_utils::{CapturingUsageService, MockAttestationService};
    use std::time::Duration;

    for reason in [
        StopReason::Incomplete,
        StopReason::ProviderError,
        StopReason::ClientDisconnect,
    ] {
        let usage = Arc::new(CapturingUsageService::new());
        let budget_id = Uuid::new_v4();
        let service = NativeResponsesService {
            models_service: Arc::new(crate::models::ports::MockModelsServiceTrait::new()),
            completion_service: Arc::new(
                crate::completions::ports::MockCompletionServiceTrait::new(),
            ),
            inference_provider_pool: Arc::new(InferenceProviderPool::new(None, Default::default())),
            usage_service: usage.clone(),
            attestation_service: Arc::new(MockAttestationService),
        };
        let response = json!({"type":"response.in_progress","response":{
            "id":"resp_partial","status":"in_progress",
            "usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15,
                "input_tokens_details":{"cached_tokens":2,"cache_write_tokens":3}}
        }});
        let mut events = vec![Ok(Bytes::from(format!("data: {response}\n\n")))];
        if reason == StopReason::ProviderError {
            events.push(Err(CompletionError::CompletionError(
                "upstream interrupted".into(),
            )));
        }
        let mut stream = UsageStream {
            inner: Box::pin(futures_util::stream::iter(events)),
            parser: EventUsage::default(),
            digest: Sha256::new(),
            billing: Some(Billing {
                budget_id: Some(budget_id),
                service,
                request_hash: String::new(),
                organization_id: Uuid::new_v4(),
                workspace_id: Uuid::new_v4(),
                api_key_id: Uuid::new_v4(),
                model_id: Uuid::new_v4(),
                inference_type: InferenceType::ChatCompletionStream,
                attribution: ProviderAttribution::default(),
            }),
            guard: None,
            runtime: tokio::runtime::Handle::current(),
            done: false,
        };
        assert!(stream.next().await.unwrap().is_ok());
        assert!(!stream.parser.usage.terminal);
        if reason != StopReason::ClientDisconnect {
            assert!(stream.next().await.unwrap().is_err());
            assert!(stream.next().await.is_none());
        }
        drop(stream);

        tokio::time::timeout(Duration::from_secs(2), async {
            while usage.get_requests().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("observed usage must be billed even without a terminal event");
        let records = usage.get_requests();
        assert_eq!(records.len(), 1);
        let recorded = &records[0];
        assert_eq!(recorded.budget_id, Some(budget_id));
        assert_eq!(recorded.input_tokens, 10);
        assert_eq!(recorded.output_tokens, 5);
        assert_eq!(recorded.cache_read_tokens, 2);
        assert_eq!(recorded.profiled_cache_write_tokens, 3);
        assert_eq!(recorded.stop_reason, Some(reason));
    }
}
