use super::*;

fn function(name: &str) -> Value {
    json!({
        "type": "function", "name": name, "description": "Read the supplied key.",
        "strict": false,
        "parameters": {"type": "object", "properties": {"key": {"type": "string"}}, "required": ["key"]}
    })
}

fn custom(name: &str) -> Value {
    json!({"type": "custom", "name": name, "description": "Execute in the client."})
}

fn namespace(name: &str, tools: Vec<Value>) -> Value {
    json!({"type": "namespace", "name": name, "description": format!("Only use {name} for its own records."), "tools": tools})
}

fn request(tools: Vec<Value>) -> Value {
    json!({"model": "test", "store": false, "input": "Synthetic task.", "tools": tools})
}

fn alias(request: &Value, index: usize) -> String {
    request["tools"][index]["name"]
        .as_str()
        .unwrap()
        .to_string()
}

fn call(name: &str, arguments: &str, status: &str) -> Value {
    json!({
        "type": "function_call", "id": "fc_test", "call_id": "call_test",
        "name": name, "arguments": arguments, "status": status,
    })
}

#[test]
fn lite_prefix_promotes_tools_without_reordering_context() {
    let messages = json!([
        {"type": "message", "role": "developer", "content": "Synthetic guidance."},
        {"type": "message", "role": "user", "content": "Synthetic request."}
    ]);
    let mut input = vec![json!({
        "type": "additional_tools", "id": "at_test", "role": "developer",
        "tools": [namespace("clock", vec![function("read")])]
    })];
    input.extend(messages.as_array().unwrap().iter().cloned());
    let (normalized, adapter) = normalize_request(json!({
        "model": "test", "store": false, "instructions": "Retain this.",
        "input": input, "tool_choice": "auto", "parallel_tool_calls": false
    }))
    .unwrap();
    assert_eq!(normalized["input"], messages);
    assert_eq!(normalized["instructions"], "Retain this.");
    assert_eq!(normalized["tool_choice"], "auto");
    assert_eq!(normalized["parallel_tool_calls"], false);
    assert_eq!(normalized["tools"].as_array().unwrap().len(), 1);
    assert_eq!(normalized["tools"][0]["type"], "function");
    let description = normalized["tools"][0]["description"].as_str().unwrap();
    assert!(description.contains("Only use clock for its own records."));
    assert!(description.contains("clock.read"));
    assert!(description.contains("Read the supplied key."));
    let restored = adapter
        .response(json!({"output": [], "tools": normalized["tools"]}))
        .unwrap();
    assert_eq!(
        restored["tools"],
        json!([namespace("clock", vec![function("read")])])
    );
}

#[test]
fn inline_tools_must_be_a_leading_developer_declaration() {
    let prefix =
        json!({"type": "additional_tools", "role": "developer", "tools": [custom("exec")]});
    let message = json!({"role": "user", "content": "test"});
    for input in [
        json!([message, prefix]),
        json!([{"type": "additional_tools", "role": "user", "tools": []}]),
        json!([{"type": "additional_tools", "role": "developer", "tools": {}}]),
    ] {
        assert!(normalize_request(json!({"model": "test", "input": input})).is_err());
        assert!(normalize_native_request(json!({"model": "test", "input": input})).is_err());
    }
}

#[test]
fn namespace_aliases_are_distinct_and_stable_across_catalog_changes() {
    let left = namespace("left", vec![function("lookup")]);
    let right = namespace("right", vec![function("lookup")]);
    let (original, _) = normalize_request(request(vec![left.clone(), right.clone()])).unwrap();
    let (changed, _) = normalize_request(request(vec![function("new_tool"), right, left])).unwrap();
    assert_ne!(alias(&original, 0), alias(&original, 1));
    assert_eq!(alias(&original, 0), alias(&changed, 2));
    assert_eq!(alias(&original, 1), alias(&changed, 1));
    assert_ne!(alias(&original, 0), "lookup");
    assert!(alias(&original, 0).len() <= 64);
    assert_eq!(
        original["tools"][0]["parameters"],
        function("lookup")["parameters"]
    );
    assert_eq!(original["tools"][0]["strict"], false);
}

#[test]
fn duplicate_tools_and_deliberate_alias_collisions_are_rejected() {
    let grouped = namespace("archive", vec![function("lookup")]);
    let (normalized, _) = normalize_request(request(vec![grouped.clone()])).unwrap();
    let colliding = function(&alias(&normalized, 0));
    for tools in [
        vec![grouped.clone(), grouped.clone()],
        vec![colliding.clone(), grouped.clone()],
        vec![grouped, colliding],
    ] {
        assert!(normalize_request(request(tools)).is_err());
    }
}

#[test]
fn codex_exec_grammar_requires_nonempty_string_input() {
    let mut exec = custom("exec");
    exec["format"] = json!({"type": "grammar", "syntax": "lark", "definition": CODEX_EXEC_GRAMMAR});
    let (normalized, adapter) =
        normalize_request(request(vec![namespace("functions", vec![exec])])).unwrap();
    let name = alias(&normalized, 0);
    assert_eq!(
        normalized["tools"][0]["parameters"]["properties"]["input"]["minLength"],
        1
    );
    assert!(adapter
        .validate_arguments(&name, r#"{"input":"text('ok');\n"}"#)
        .is_ok());
    for invalid in ["not JSON", "{}", r#"{"input":42}"#, r#"{"input":""}"#] {
        assert!(
            adapter.validate_arguments(&name, invalid).is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn custom_text_allows_empty_input_but_other_grammars_require_native() {
    for format in [Value::Null, json!({"type": "text"})] {
        let mut tool = custom("run");
        tool["format"] = format;
        let (normalized, adapter) = normalize_request(request(vec![tool])).unwrap();
        assert!(adapter
            .validate_arguments(&alias(&normalized, 0), r#"{"input":""}"#)
            .is_ok());
    }
    for format in [
        json!({"type": "grammar", "syntax": "regex", "definition": "[a-z]+"}),
        json!({"type": "grammar", "syntax": "lark", "definition": "start: /[a-z]+/"}),
        json!({"type": "grammar", "syntax": "lark", "definition": 5}),
        json!({"type": "unknown"}),
    ] {
        let mut tool = custom("run");
        tool["format"] = format;
        assert!(normalize_request(request(vec![tool])).is_err());
    }
}

#[test]
fn custom_history_replays_without_its_tool_in_the_current_catalog() {
    let (declared, _) =
        normalize_request(request(vec![namespace("functions", vec![custom("exec")])])).unwrap();
    let script = "text('line 1');\ntext('line 2');";
    let mut body = request(vec![function("new_tool")]);
    body["input"] = json!([
        {"type": "custom_tool_call", "id": "ctc_old", "call_id": "call_old", "namespace": "functions", "name": "exec", "input": script, "status": "completed"},
        {"type": "custom_tool_call_output", "call_id": "call_old", "output": [{"type": "input_text", "text": "line 1"}, {"type": "input_text", "text": "line 2"}]}
    ]);
    let (normalized, adapter) = normalize_request(body).unwrap();
    assert_eq!(normalized["tools"].as_array().unwrap().len(), 1);
    assert_eq!(normalized["tools"][0]["name"], "new_tool");
    let history = &normalized["input"][0];
    assert_eq!(history["type"], "function_call");
    assert_eq!(history["name"], declared["tools"][0]["name"]);
    assert_eq!(history["call_id"], "call_old");
    assert!(history.get("input").is_none());
    assert!(history.get("namespace").is_none());
    let arguments: Value = serde_json::from_str(history["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(arguments, json!({"input": script}));
    assert_eq!(
        normalized["input"][1],
        json!({"type": "function_call_output", "call_id": "call_old", "output": "line 1\nline 2"})
    );
    let restored = adapter.response(json!({"output": [history]})).unwrap();
    assert_eq!(restored["output"][0], *history);
    assert!(adapter
        .validate_arguments(history["name"].as_str().unwrap(), "{}")
        .is_ok());

    // A client may also omit tools entirely when it only replays old results.
    let (without_catalog, adapter) = normalize_request(json!({"model": "test", "input": [
        {"type": "custom_tool_call", "call_id": "call_old", "name": "exec", "namespace": "functions", "input": script},
        {"type": "custom_tool_call_output", "call_id": "call_old", "output": "ok"}
    ]})).unwrap();
    assert!(without_catalog.get("tools").is_none());
    assert_eq!(
        without_catalog["input"][0]["name"],
        declared["tools"][0]["name"]
    );
    let output = json!({"output": [without_catalog["input"][0]]});
    assert_eq!(adapter.response(output.clone()).unwrap(), output);
}

#[test]
fn raw_alias_calls_replay_alongside_their_original_tool_identities() {
    for (tool, history) in [
        (
            custom("exec"),
            json!({"type":"custom_tool_call", "name":"exec", "input":"text(1)"}),
        ),
        (
            namespace("functions", vec![custom("exec")]),
            json!({"type":"custom_tool_call", "name":"exec", "namespace":"functions", "input":"text(1)"}),
        ),
        (
            namespace("archive", vec![function("lookup")]),
            json!({"type":"function_call", "name":"lookup", "namespace":"archive", "arguments":"{}"}),
        ),
    ] {
        let (declared, _) = normalize_request(request(vec![tool.clone()])).unwrap();
        let name = alias(&declared, 0);
        let raw = call(&name, "not JSON", "completed");
        // A tool can also be re-enabled after the client rejected a raw alias.
        for tools in [vec![function("new_tool")], vec![tool]] {
            for input in [json!([history, raw]), json!([raw, history])] {
                let (normalized, _) = normalize_request(json!({
                    "model":"test", "tools":tools, "input":input
                }))
                .unwrap();
                assert!(normalized["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|item| item["name"] == name));
                let replayed = normalized["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|item| item["id"] == "fc_test")
                    .unwrap();
                assert_eq!(*replayed, raw);
            }
        }
    }
}

#[test]
fn regular_function_arguments_and_unrelated_items_are_preserved_verbatim() {
    let arguments = " \n{ \"key\" : \"上海\" }\t";
    let message = json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Reading."}], "phase": "commentary"});
    let mut body = request(vec![namespace("archive", vec![function("lookup")])]);
    body["input"] = json!([message, {"type": "function_call", "call_id": "old", "name": "lookup", "namespace": "archive", "arguments": arguments}]);
    let (normalized, adapter) = normalize_request(body).unwrap();
    assert_eq!(normalized["input"][0], message);
    assert_eq!(normalized["input"][1]["arguments"], arguments);
    let internal = call(&alias(&normalized, 0), arguments, "completed");
    let restored = adapter
        .response(json!({"output": [internal, message]}))
        .unwrap();
    assert_eq!(restored["output"][0]["type"], "function_call");
    assert_eq!(restored["output"][0]["name"], "lookup");
    assert_eq!(restored["output"][0]["namespace"], "archive");
    assert_eq!(restored["output"][0]["arguments"], arguments);
    assert_eq!(restored["output"][1], message);
    let mut stream = adapter.stream();
    stream.event(json!({"type": "response.output_item.added", "item": call(&alias(&normalized, 0), "", "in_progress")})).unwrap();
    let event = json!({"type": "response.function_call_arguments.delta", "item_id": "fc_test", "delta": arguments});
    assert_eq!(stream.event(event.clone()).unwrap(), event);
}

#[test]
fn named_choices_resolve_declared_tools_and_restore_the_external_identity() {
    let mut body = request(vec![function("lookup")]);
    body["tool_choice"] = json!({"type": "function", "name": "lookup"});
    let (normalized, adapter) = normalize_request(body).unwrap();
    assert_eq!(
        normalized["tool_choice"],
        json!({"type": "function", "function": {"name": "lookup"}})
    );
    let response = adapter
        .response(json!({"output": [], "tool_choice": normalized["tool_choice"]}))
        .unwrap();
    assert_eq!(
        response["tool_choice"],
        json!({"type": "function", "name": "lookup"})
    );

    let mut body = request(vec![namespace("functions", vec![custom("exec")])]);
    body["tool_choice"] = json!({"type": "custom", "namespace": "functions", "name": "exec"});
    let (normalized, adapter) = normalize_request(body).unwrap();
    assert_eq!(
        normalized["tool_choice"],
        json!({"type": "function", "function": {"name": alias(&normalized, 0)}})
    );
    let response = adapter
        .response(json!({"output": [], "tool_choice": normalized["tool_choice"]}))
        .unwrap();
    assert_eq!(
        response["tool_choice"],
        json!({"type": "custom", "namespace": "functions", "name": "exec"})
    );
}

#[test]
fn unknown_or_undeclared_tool_choices_are_not_downgraded_to_auto() {
    for choice in [json!("auto"), json!("none"), json!("required"), Value::Null] {
        let mut body = request(vec![function("lookup")]);
        body["tool_choice"] = choice.clone();
        assert_eq!(normalize_request(body).unwrap().0["tool_choice"], choice);
    }
    for choice in [
        json!("sometimes"),
        json!(true),
        json!(1),
        json!([]),
        json!({"type": "function", "name": "missing"}),
        json!({"type": "custom", "name": "lookup"}),
        json!({"type": "web_search"}),
        json!({"name": "lookup"}),
    ] {
        let mut body = request(vec![function("lookup")]);
        body["tool_choice"] = choice.clone();
        assert!(normalize_request(body).is_err(), "invalid choice: {choice}");
    }
}

#[test]
fn custom_items_and_events_restore_raw_input_and_namespace() {
    let tools = vec![namespace("functions", vec![custom("exec")])];
    let (normalized, adapter) = normalize_request(request(tools.clone())).unwrap();
    let name = alias(&normalized, 0);
    let input = "text('hello');\n// Keep whitespace.\n";
    let arguments = json!({"input": input}).to_string();
    let mut stream = adapter.stream();
    let added = stream.event(json!({"type": "response.output_item.added", "output_index": 2, "item": call(&name, "", "in_progress")})).unwrap();
    assert_eq!(added["item"]["type"], "custom_tool_call");
    assert_eq!(added["item"]["namespace"], "functions");
    assert_eq!(added["item"]["name"], "exec");
    assert_eq!(added["item"]["input"], "");
    assert!(added["item"].get("arguments").is_none());
    let delta = stream.event(json!({"type": "response.function_call_arguments.delta", "item_id": "fc_test", "output_index": 2, "sequence_number": 3, "delta": arguments})).unwrap();
    assert_eq!(
        delta,
        json!({"type": "response.custom_tool_call_input.delta", "item_id": "fc_test", "output_index": 2, "sequence_number": 3, "delta": input})
    );
    let done = stream.event(json!({"type": "response.function_call_arguments.done", "item_id": "fc_test", "output_index": 2, "sequence_number": 4, "arguments": arguments})).unwrap();
    assert_eq!(
        done,
        json!({"type": "response.custom_tool_call_input.done", "item_id": "fc_test", "output_index": 2, "sequence_number": 4, "input": input})
    );
    let item_done = stream.event(json!({"type": "response.output_item.done", "item": call(&name, &arguments, "completed")})).unwrap();
    let expected = json!({"type": "custom_tool_call", "id": "fc_test", "call_id": "call_test", "namespace": "functions", "name": "exec", "input": input, "status": "completed"});
    assert_eq!(item_done["item"], expected);
    let completed = stream.event(json!({"type": "response.completed", "response": {"status": "completed", "output": [call(&name, &arguments, "completed")], "tools": normalized["tools"]}})).unwrap();
    assert_eq!(completed["response"]["output"], json!([expected]));
    assert_eq!(completed["response"]["tools"], json!(tools));
}

#[test]
fn malformed_custom_wrappers_return_errors_instead_of_empty_successes() {
    let (normalized, adapter) = normalize_request(request(vec![custom("exec")])).unwrap();
    let name = alias(&normalized, 0);
    for arguments in [r#"{"input":42}"#, "not JSON", "{}"] {
        assert!(adapter
            .response(json!({"output": [call(&name, arguments, "completed")]}))
            .is_err());
        let mut stream = adapter.stream();
        stream.event(json!({"type": "response.output_item.added", "item": call(&name, "", "in_progress")})).unwrap();
        assert!(stream.event(json!({"type": "response.function_call_arguments.delta", "item_id": "fc_test", "delta": arguments})).is_err());
        assert!(stream.event(json!({"type": "response.function_call_arguments.done", "item_id": "fc_test", "arguments": arguments})).is_err());
        assert!(stream.event(json!({"type": "response.output_item.done", "item": call(&name, arguments, "completed")})).is_err());
        assert!(stream.event(json!({"type": "response.completed", "response": {"output": [call(&name, arguments, "completed")]}})).is_err());
    }
}

#[test]
fn nontext_tool_output_is_rejected_instead_of_discarded() {
    for output in [
        json!([{"type": "input_image", "image_url": "data:image/png;base64,AA=="}]),
        json!([{"type": "input_text", "text": 1}]),
    ] {
        assert!(normalize_request(json!({"model": "test", "input": [
            {"type": "custom_tool_call_output", "call_id": "call_old", "output": output}
        ]}))
        .is_err());
    }
}

#[test]
fn native_keeps_custom_constraints_and_reasoning_while_promoting_lite_tools() {
    let mut tool = custom("run");
    tool["format"] = json!({"type": "grammar", "syntax": "regex", "definition": "[a-z]+"});
    let tools = vec![namespace("client", vec![tool])];
    let reasoning = json!({"type": "reasoning", "id": "rs_old", "summary": [], "encrypted_content": "opaque_fixture"});
    let call = json!({"type": "custom_tool_call", "name": "run", "namespace": "client", "call_id": "call_old", "input": "hello"});
    let output =
        json!({"type": "custom_tool_call_output", "call_id": "call_old", "output": "Done"});
    let normalized = normalize_native_request(json!({"model": "test", "store": false, "input": [
        {"type": "additional_tools", "role": "developer", "tools": tools}, reasoning, call, output
    ]}))
    .unwrap();
    assert_eq!(normalized["tools"], json!(tools));
    assert_eq!(normalized["input"], json!([reasoning, call, output]));
}

#[test]
fn mcp_tools_are_detected_wherever_tools_can_be_declared() {
    let mcp =
        json!({"type": "mcp", "server_label": "remote", "server_url": "https://mcp.invalid/"});
    for body in [
        request(vec![mcp.clone()]),
        request(vec![function("lookup"), mcp.clone()]),
        request(vec![namespace("client", vec![mcp.clone()])]),
        request(vec![namespace(
            "outer",
            vec![namespace("inner", vec![mcp.clone()])],
        )]),
        json!({"model": "test", "input": [
            {"type": "additional_tools", "role": "developer", "tools": [mcp.clone()]},
            {"role": "user", "content": "Synthetic task."}
        ]}),
    ] {
        assert!(contains_mcp_tool(&body), "{body}");
    }
    for body in [
        request(vec![
            function("mcp"),
            custom("mcp"),
            json!({"type": "web_search"}),
        ]),
        request(vec![namespace("mcp", vec![function("lookup")])]),
        json!({"model": "test", "input": [
            {"type": "mcp_list_tools", "tools": [{"type": "mcp"}]},
            {"role": "user", "content": "mcp"}
        ]}),
        json!({"model": "test", "input": "Synthetic task.", "tools": null}),
    ] {
        assert!(!contains_mcp_tool(&body), "{body}");
    }
}

#[test]
fn unsupported_hosted_tools_remain_rejected_on_both_transports() {
    for kind in [
        "file_search",
        "mcp",
        "code_interpreter",
        "tool_search",
        "image_generation",
    ] {
        let hosted = json!({"type": kind, "name": "hosted"});
        for tools in [
            vec![hosted.clone()],
            vec![namespace("client", vec![hosted])],
        ] {
            assert!(normalize_request(request(tools.clone())).is_err());
            assert!(normalize_native_request(request(tools.clone())).is_err());
            assert!(normalize_native_request(json!({"model": "test", "input": [
                {"type": "additional_tools", "role": "developer", "tools": tools}
            ]}))
            .is_err());
        }
    }
}

#[test]
fn typed_search_preserves_configuration_alongside_namespaced_client_tools() {
    for count in [None, Some(0), Some(5), Some(20)] {
        let mut search = json!({
            "type": "web_search", "filters": {"allowed_domains": ["example.com"]},
            "search_context_size": "small", "user_location": {"type": "approximate", "country": "US"}
        });
        if let Some(count) = count {
            search["count"] = json!(count);
        }
        let tools = vec![
            search.clone(),
            namespace("client", vec![custom("exec")]),
            json!({"type": "web_context_search"}),
        ];
        let (normalized, adapter) = normalize_request(request(tools.clone())).unwrap();
        assert_eq!(normalized["tools"][0], search);
        assert_eq!(
            normalized["tools"][2],
            json!({"type": "web_context_search"})
        );
        assert_eq!(normalized["tools"][1]["type"], "function");
        let typed: crate::responses::models::CreateResponseRequest =
            serde_json::from_value(normalized.clone()).unwrap();
        assert!(typed.validate().is_ok());
        assert!(typed.validate_stateless().is_ok());
        let restored = adapter
            .response(json!({"output": [], "tools": normalized["tools"]}))
            .unwrap();
        assert_eq!(restored["tools"], json!(tools));
        assert!(normalize_native_request(request(tools)).is_err());
    }
}

#[test]
fn search_is_not_allowed_inside_client_namespaces() {
    for kind in ["web_search", "web_context_search"] {
        let search = json!({"type": kind});
        assert!(normalize_request(request(vec![search.clone()])).is_ok());
        let tools = vec![namespace("client", vec![search])];
        assert!(normalize_request(request(tools.clone())).is_err());
        assert!(normalize_native_request(request(tools.clone())).is_err());
        assert!(normalize_native_request(json!({"model": "test", "input": [
            {"type": "additional_tools", "role": "developer", "tools": tools}
        ]}))
        .is_err());
    }
}

#[test]
fn native_search_accepts_only_web_search_without_relaxing_client_tool_validation() {
    let search = json!({"type":"web_search", "external_web_access":false});
    let body = request(vec![search.clone(), function("lookup")]);
    assert_eq!(normalize_native_request(body.clone()).unwrap(), body);
    let promoted = normalize_native_request(json!({"model":"test", "input":[
        {"type":"additional_tools", "role":"developer", "tools":[search]},
        {"role":"user", "content":"Search the web."}
    ]}))
    .unwrap();
    assert_eq!(promoted["tools"], json!([search]));
    assert_eq!(
        promoted["input"],
        json!([{"role":"user", "content":"Search the web."}])
    );
    for invalid in [
        json!({"type":"web_context_search"}),
        json!({"type":"web_search_preview"}),
        json!({"type":"function"}),
        json!({"type":"custom", "name":" "}),
        namespace("client", vec![json!({"type":"function"})]),
        namespace(
            "outer",
            vec![namespace(
                "inner",
                vec![json!({"type":"mcp", "server_label":"remote"})],
            )],
        ),
    ] {
        let tools = vec![search.clone(), invalid];
        assert!(normalize_native_request(request(tools.clone())).is_err());
        assert!(normalize_native_request(json!({"model":"test", "input":[
            {"type":"additional_tools", "role":"developer", "tools":tools}
        ]}))
        .is_err());
    }
}

#[test]
fn search_name_collisions_are_rejected_without_reserving_client_names() {
    for kind in ["web_search", "web_context_search"] {
        let search = json!({"type": kind});
        for tools in [
            vec![search.clone(), function(kind)],
            vec![function(kind), search.clone()],
        ] {
            assert!(normalize_request(request(tools))
                .unwrap_err()
                .contains("conflicts"));
        }
        let (client_only, _) = normalize_request(request(vec![function(kind)])).unwrap();
        assert_eq!(client_only["tools"][0], function(kind));
        let (namespaced, _) = normalize_request(request(vec![
            search.clone(),
            namespace("client", vec![function(kind)]),
        ]))
        .unwrap();
        assert_ne!(namespaced["tools"][1]["name"], kind);
        let (custom_tool, _) =
            normalize_request(request(vec![search.clone(), custom(kind)])).unwrap();
        assert_ne!(custom_tool["tools"][1]["name"], kind);
    }
}

#[test]
fn named_function_choice_can_select_a_declared_cloud_search() {
    for kind in ["web_search", "web_context_search"] {
        for choice in [
            json!({"type": "function", "name": kind}),
            json!({"type": "function", "function": {"name": kind}}),
        ] {
            let mut body = request(vec![json!({"type": kind})]);
            body["tool_choice"] = choice;
            let (normalized, _) = normalize_request(body).unwrap();
            assert_eq!(
                normalized["tool_choice"],
                json!({"type": "function", "function": {"name": kind}})
            );
        }
    }
}

#[test]
fn completed_search_replay_keeps_assistant_context_without_tool_history() {
    let answer = json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "An answer with sources.", "annotations": []}]});
    let next = json!({"role": "user", "content": "A follow-up."});
    let marker = json!({"type": "web_search_call", "id": "ws_old", "status": "completed", "action": {"type": "search", "query": "Synthetic query"}});
    let (normalized, _) =
        normalize_request(json!({"model": "test", "input": [marker, answer, next]})).unwrap();
    assert_eq!(normalized["input"], json!([answer, next]));
    let typed: crate::responses::models::CreateResponseRequest =
        serde_json::from_value(normalized).unwrap();
    assert!(typed.validate().is_ok());
    assert!(typed.validate_stateless().is_ok());
    for status in ["in_progress", "failed", "searching"] {
        assert!(normalize_request(json!({"model": "test", "input": [
            {"type": "web_search_call", "id": "ws_old", "status": status}
        ]}))
        .is_err());
    }
}
