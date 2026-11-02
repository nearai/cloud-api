use crate::common::*;
use inference_providers::mock::ResponseTemplate;
use serde_json::{json, Value};

fn events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).expect("Responses SSE JSON"))
        .collect()
}

fn failed_response(events: &[Value]) -> &Value {
    let first = events.first().expect("created event");
    assert_eq!(first["type"], "response.created");
    let failed = events.last().expect("failed event");
    assert_eq!(failed["type"], "response.failed");
    assert_eq!(failed["response"]["id"], first["response"]["id"]);
    assert_eq!(failed["response"]["status"], "failed");
    assert!(events
        .iter()
        .all(|event| event["type"] != "response.completed"));
    for (sequence, event) in events.iter().enumerate() {
        assert_eq!(event["sequence_number"], sequence);
    }
    failed
}

#[tokio::test]
async fn responses_failure_has_standard_envelope_and_preserves_nonstream_http_error() {
    let (server, _pool, mock, _db) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    mock.set_error_override(Some(inference_providers::CompletionError::HttpError {
        status_code: 400,
        message: "Unsupported prompt format".to_string(),
        is_external: false,
    }))
    .await;

    for stream in [true, false] {
        let result = server
            .post("/v1/responses")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&json!({
                "model": model,
                "input": "Hello",
                "store": false,
                "stream": stream,
            }))
            .await;
        if stream {
            assert_eq!(result.status_code(), 200);
            let events = events(&result.text());
            let failed = failed_response(&events);
            assert_eq!(failed["response"]["error"]["code"], "invalid_prompt");
            assert_eq!(failed["response"]["error"]["type"], "invalid_request_error");
            assert!(failed["response"]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Unsupported prompt format"));
            assert_eq!(failed["status_code"], 400);
            assert!(failed["response"]["output"].as_array().unwrap().is_empty());
        } else {
            assert_eq!(result.status_code(), 400);
            let error = result.json::<api::models::ErrorResponse>();
            assert_eq!(error.error.r#type, "invalid_request_error");
            assert!(error.error.message.contains("Unsupported prompt format"));
        }
    }
}

#[tokio::test]
async fn responses_failure_retains_partial_output_and_usage_without_replaying_history() {
    let (server, _pool, mock, _db) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    mock.set_default_response(
        ResponseTemplate::new("partial output").with_stream_error_after(
            1,
            inference_providers::CompletionError::HttpError {
                status_code: 429,
                message: "Too many requests".to_string(),
                is_external: false,
            },
        ),
    )
    .await;

    for stream in [true, false] {
        let result = server
            .post("/v1/responses")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&json!({
                "model": model,
                "input": [
                    {"role": "user", "content": "Previous question"},
                    {"role": "assistant", "content": "Historical output"},
                    {"role": "user", "content": "Continue"}
                ],
                "store": false,
                "stream": stream,
            }))
            .await;
        if stream {
            assert_eq!(result.status_code(), 200);
            let events = events(&result.text());
            let failed = failed_response(&events);
            assert_eq!(failed["response"]["error"]["code"], "rate_limit_exceeded");
            assert_eq!(failed["response"]["error"]["message"], "Too many requests");
            assert_eq!(failed["status_code"], 429);
            assert_eq!(failed["response"]["output"].as_array().unwrap().len(), 1);
            assert_eq!(
                failed["response"]["output"][0]["content"][0]["text"],
                "partial"
            );
            assert!(
                failed["response"]["usage"]["input_tokens"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
            assert_eq!(failed["response"]["usage"]["output_tokens"], 1);
            assert_eq!(failed["usage"], failed["response"]["usage"]);
        } else {
            assert_eq!(result.status_code(), 429);
            let error = result.json::<api::models::ErrorResponse>();
            assert_eq!(error.error.r#type, "rate_limit_exceeded");
        }
    }
}
