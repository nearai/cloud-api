use super::*;
use crate::responses::service_helpers::{EventEmitter, ResponseStreamContext, UsageTracker};
use futures::StreamExt;

#[tokio::test]
async fn interrupted_reasoning_and_custom_arguments_keep_the_original_failure() {
    use serde_json::json;

    let (body, adapter) = crate::responses::client_tools::normalize_request(json!({
        "model":"test", "store":false,
        "tools":[{"type":"custom", "name":"exec", "format":{"type":"text"}}]
    }))
    .unwrap();
    let mut request: models::CreateResponseRequest = serde_json::from_value(body).unwrap();
    request.client_tool_adapter = adapter;
    let models::ResponseTool::Function { name, .. } = &request.tools.as_ref().unwrap()[0] else {
        panic!("custom tool must be adapted to a Chat function");
    };
    let partial_arguments = "{\"input\":\"unfinished";
    assert!(request
        .client_tool_adapter
        .validate_arguments(name, partial_arguments)
        .is_err());

    let (responses, items) = transient::repositories();
    let key = Uuid::new_v4();
    let mut response = responses
        .create(
            crate::workspace::WorkspaceId(Uuid::new_v4()),
            key,
            request.clone(),
        )
        .await
        .unwrap();
    let response_id = ResponseServiceImpl::extract_response_uuid(&response).unwrap();
    let mut ctx = ResponseStreamContext::new(
        response_id.clone(),
        key,
        None,
        response.id.clone(),
        None,
        response.created_at,
        response.model.clone(),
        UsageTracker::new(),
    );
    let reasoning = "Partial reasoning before interruption.";
    let chunk = |delta, usage| {
        Ok(inference_providers::SSEEvent {
            raw_bytes: Default::default(),
            raw_passthrough: false,
            chunk: Some(inference_providers::StreamChunk::Chat(
                serde_json::from_value(json!({
                    "id":"chat_partial", "object":"chat.completion.chunk", "created":0,
                    "model":"test", "choices":[{"index":0, "delta":delta}], "usage":usage,
                }))
                .unwrap(),
            )),
        })
    };
    let chunks = vec![
        chunk(
            json!({"reasoning_content":reasoning}),
            json!({
                "prompt_tokens":13, "completion_tokens":5, "total_tokens":18,
                "prompt_tokens_details":{"cached_tokens":3},
            }),
        ),
        chunk(
            json!({"tool_calls":[{
                "index":0, "id":"call_partial", "function":{"name":name,"arguments":partial_arguments}
            }]}),
            json!(null),
        ),
        Err(inference_providers::CompletionError::HttpError {
            status_code: 429,
            message: "provider interrupted generation".to_string(),
            is_external: false,
        }),
    ];
    let mut stream: Pin<
        Box<
            dyn Stream<
                    Item = Result<
                        inference_providers::SSEEvent,
                        inference_providers::CompletionError,
                    >,
                > + Send,
        >,
    > = Box::pin(futures::stream::iter(chunks));
    let (tx, rx) = futures::channel::mpsc::unbounded();
    let mut emitter = EventEmitter::new(tx);
    emitter
        .emit_created(&mut ctx, response.clone())
        .await
        .unwrap();
    let result = ResponseServiceImpl::process_completion_stream(
        &mut stream,
        &mut emitter,
        &mut ctx,
        &items,
        &request,
        Uuid::new_v4(),
        None,
    )
    .await
    .expect("partial custom arguments must not replace the provider failure");
    assert!(result.stream_error);
    assert!(result.tool_calls.is_empty());
    let error = result
        .stream_error_cause
        .expect("original provider failure");
    assert_eq!(error.http_status_code(), 429);
    response.output = ResponseServiceImpl::select_output_items(
        items.list_by_response(response_id).await.unwrap(),
        &HashSet::new(),
    );
    assert_eq!(response.output.len(), 1);
    assert!(
        matches!(&response.output[0], models::ResponseOutputItem::Reasoning {
        status: models::ResponseItemStatus::InProgress, content, ..
    } if content == reasoning)
    );
    emitter
        .emit_failed(&mut ctx, response, &error)
        .await
        .unwrap();
    drop(emitter);
    let events: Vec<_> = rx.collect().await;
    assert!(events
        .iter()
        .all(|event| !event.event_type.ends_with(".done")
            && event.event_type != "response.completed"));
    let failed = serde_json::to_value(events.last().unwrap()).unwrap();
    assert_eq!(failed["type"], "response.failed");
    assert_eq!(failed["status_code"], 429);
    assert_eq!(failed["response"]["error"]["code"], "rate_limit_exceeded");
    assert_eq!(failed["response"]["output"][0]["status"], "in_progress");
    assert_eq!(
        failed["response"]["output"][0]["content"][0]["text"],
        reasoning
    );
    assert_eq!(failed["response"]["usage"]["input_tokens"], 13);
    assert_eq!(failed["response"]["usage"]["output_tokens"], 5);
    assert_eq!(
        failed["response"]["usage"]["input_tokens_details"]["cached_tokens"],
        3
    );
    assert!(
        failed["response"]["usage"]["output_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .unwrap()
            > 0
    );
    for (sequence, event) in events.iter().enumerate() {
        assert_eq!(event.sequence_number, Some(sequence as u64));
    }
}

#[tokio::test]
async fn failed_response_keeps_identity_sequence_partial_output_and_usage() {
    for (error, code, status_code) in [
        (
            errors::ResponseError::InvalidParams("Unsupported prompt format".to_string()),
            "invalid_prompt",
            400,
        ),
        (
            errors::ResponseError::StreamInterrupted,
            "server_error",
            500,
        ),
    ] {
        let (responses, _) = transient::repositories();
        let api_key = Uuid::new_v4();
        let mut response = responses
            .create(
                crate::workspace::WorkspaceId(Uuid::new_v4()),
                api_key,
                serde_json::from_value(serde_json::json!({"model": "test"})).unwrap(),
            )
            .await
            .unwrap();
        let mut ctx = ResponseStreamContext::new(
            ResponseServiceImpl::extract_response_uuid(&response).unwrap(),
            api_key,
            None,
            response.id.clone(),
            None,
            response.created_at,
            response.model.clone(),
            UsageTracker::new(),
        );
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let mut emitter = EventEmitter::new(tx);
        emitter
            .emit_created(&mut ctx, response.clone())
            .await
            .unwrap();
        emitter
            .emit_in_progress(&mut ctx, response.clone())
            .await
            .unwrap();
        let partial = models::ResponseOutputItem::Message {
            id: "msg_partial".to_string(),
            response_id: response.id.clone(),
            previous_response_id: None,
            next_response_ids: vec![],
            created_at: response.created_at,
            role: "assistant".to_string(),
            content: vec![models::ResponseContentItem::OutputText {
                text: "partial output".to_string(),
                annotations: vec![],
                logprobs: vec![],
            }],
            status: models::ResponseItemStatus::InProgress,
            model: response.model.clone(),
            metadata: None,
        };
        emitter
            .emit_item_added(&mut ctx, partial.clone(), partial.id().to_string())
            .await
            .unwrap();
        response.output.push(partial);
        ctx.update_usage(13, 5, 3);
        ctx.add_reasoning_tokens(2);
        emitter
            .emit_failed(&mut ctx, response, &error)
            .await
            .unwrap();
        drop(emitter);
        let events: Vec<_> = rx.collect().await;
        let failed = serde_json::to_value(events.last().unwrap()).unwrap();
        assert_eq!(failed["type"], "response.failed");
        assert_eq!(
            failed["response"]["id"],
            events[0].response.as_ref().unwrap().id
        );
        assert_eq!(failed["response"]["status"], "failed");
        assert_eq!(failed["response"]["error"]["code"], code);
        assert_eq!(
            failed["response"]["error"]["message"],
            error.response_error().message
        );
        assert_eq!(failed["status_code"], status_code);
        assert_eq!(
            failed["response"]["output"][0]["content"][0]["text"],
            "partial output"
        );
        assert_eq!(failed["response"]["usage"]["input_tokens"], 13);
        assert_eq!(failed["response"]["usage"]["output_tokens"], 5);
        assert_eq!(
            failed["response"]["usage"]["input_tokens_details"]["cached_tokens"],
            3
        );
        assert_eq!(
            failed["response"]["usage"]["output_tokens_details"]["reasoning_tokens"],
            2
        );
        assert_eq!(failed["usage"], failed["response"]["usage"]);
        assert_eq!(
            failed["error"]["message"],
            failed["response"]["error"]["message"]
        );
        assert!(failed["text"].is_null());
        for (sequence, event) in events.iter().enumerate() {
            assert_eq!(event.sequence_number, Some(sequence as u64));
        }
    }
}
