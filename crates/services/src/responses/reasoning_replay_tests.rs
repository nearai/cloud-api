use super::*;
use serde_json::json;

#[test]
fn reasoning_replay_stays_with_assistant_text_and_function_calls() {
    for preamble in [None, Some("I will look it up.")] {
        for (summary, content, expected) in [
            ("summary", "  full reasoning\n", "  full reasoning\n"),
            ("summary only", "", "summary only"),
        ] {
            let reasoning = models::ResponseOutputItem::Reasoning {
                id: "rs_previous".to_string(),
                response_id: String::new(),
                previous_response_id: None,
                next_response_ids: vec![],
                created_at: 0,
                status: models::ResponseItemStatus::Completed,
                summary: summary.to_string(),
                content: content.to_string(),
                model: "test".to_string(),
            };
            let mut input = vec![serde_json::to_value(reasoning).unwrap()];
            if let Some(text) = preamble {
                input.push(json!({"type":"message", "role":"assistant", "content":text}));
            }
            input.extend([
                json!({"type":"function_call", "call_id":"call_weather", "name":"lookup",
                    "arguments":"{ \"city\": \"上海\" }", "thought_signature":"signature"}),
                json!({"type":"function_call_output", "call_id":"call_weather", "output":"22 C"}),
            ]);
            let items: Vec<models::ResponseInputItem> =
                serde_json::from_value(json!(input)).unwrap();
            let mut messages = Vec::new();
            let mut pending_assistant = None;
            let mut pending_calls = Vec::new();
            for item in &items {
                if ResponseServiceImpl::append_replayed_function_call_item(
                    item,
                    &mut messages,
                    &mut pending_assistant,
                    &mut pending_calls,
                ) {
                    continue;
                }
                let models::ResponseInputItem::Message {
                    role,
                    content: models::ResponseContent::Text(text),
                    ..
                } = item
                else {
                    panic!("expected assistant preamble");
                };
                ResponseServiceImpl::append_replayed_assistant_message(
                    crate::completions::ports::CompletionMessage {
                        role: role.clone(),
                        content: json!(text),
                        tool_call_id: None,
                        tool_calls: None,
                        reasoning_content: None,
                    },
                    &mut messages,
                    &mut pending_assistant,
                    &mut pending_calls,
                );
            }
            ResponseServiceImpl::flush_replayed_function_calls(
                &mut messages,
                &mut pending_assistant,
                &mut pending_calls,
            );
            assert_eq!(
                messages.len(),
                2,
                "reasoning and calls form one assistant turn"
            );
            assert_eq!(messages[0].role, "assistant");
            assert_eq!(messages[0].reasoning_content.as_deref(), Some(expected));
            assert_eq!(messages[0].content, json!(preamble.unwrap_or_default()));
            let calls = messages[0].tool_calls.as_ref().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].id, "call_weather");
            assert_eq!(calls[0].arguments, "{ \"city\": \"上海\" }");
            assert_eq!(calls[0].thought_signature.as_deref(), Some("signature"));
            assert_eq!(messages[1].role, "tool");
            assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_weather"));
            assert_eq!(messages[1].content, "22 C");
            assert!(messages[1].reasoning_content.is_none());
        }
    }
}

#[tokio::test]
async fn replayed_reasoning_is_context_only_and_never_new_output() {
    let (responses, items) = transient::repositories();
    let key = Uuid::new_v4();
    let request = serde_json::from_value(json!({"model":"test", "store":false})).unwrap();
    let response = responses
        .create(crate::workspace::WorkspaceId(Uuid::new_v4()), key, request)
        .await
        .unwrap();
    let response_id = ResponseServiceImpl::extract_response_uuid(&response).unwrap();
    let input = serde_json::from_value(json!([
        {"type":"reasoning", "summary":[], "content":[{"type":"reasoning_text", "text":"prior reasoning"}]}
    ])).unwrap();
    let stored = ResponseServiceImpl::store_input_as_response_items(
        &items,
        response_id.clone(),
        key,
        None,
        &input,
        "test",
        None,
    )
    .await
    .unwrap();
    assert!(stored.is_empty());
    assert!(items
        .list_by_response(response_id)
        .await
        .unwrap()
        .is_empty());
}
