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
    assert_eq!(restored["output"][0]["type"], "custom_tool_call");
    assert_eq!(restored["output"][0]["namespace"], "functions");
    assert_eq!(restored["output"][0]["name"], "exec");
    assert_eq!(restored["output"][0]["input"], script);

    // A client may also omit tools entirely when it only replays old results.
    let (without_catalog, _) = normalize_request(json!({"model": "test", "input": [
        {"type": "custom_tool_call", "call_id": "call_old", "name": "exec", "namespace": "functions", "input": script},
        {"type": "custom_tool_call_output", "call_id": "call_old", "output": "ok"}
    ]})).unwrap();
    assert!(without_catalog.get("tools").is_none());
    assert_eq!(
        without_catalog["input"][0]["name"],
        declared["tools"][0]["name"]
    );
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
    let normalized = normalize_native_request(json!({"model": "test", "store": false, "input": [
        {"type": "additional_tools", "role": "developer", "tools": tools}, reasoning, call
    ]}))
    .unwrap();
    assert_eq!(normalized["tools"], json!(tools));
    assert_eq!(normalized["input"], json!([reasoning, call]));
}

#[test]
fn native_and_chat_validation_reject_hosted_tools_even_inside_namespaces() {
    for kind in [
        "web_search",
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
