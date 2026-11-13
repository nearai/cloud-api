use crate::common::*;
use inference_providers::{mock::ResponseTemplate, CompletionError};
use serde_json::{json, Value};

fn json_events(body: &str) -> Vec<Value> {
    body.replace("\r\n", "\n")
        .split("\n\n")
        .filter_map(|event| {
            let data = event
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .collect::<Vec<_>>()
                .join("\n");
            (!data.is_empty()).then(|| {
                serde_json::from_str(&data)
                    .expect("failed streams must contain independently parseable JSON events")
            })
        })
        .collect()
}

#[tokio::test]
async fn interrupted_chat_and_text_streams_end_with_one_error_without_success_artifacts() {
    let (server, _pool, mock, database) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    for upstream_error in [false, true] {
        for (endpoint, options) in [
            ("chat/completions", json!({"include_usage": true})),
            ("chat/completions", json!({"continuous_usage_stats": true})),
            ("completions", json!({})),
        ] {
            let template = ResponseTemplate::new("partial output before interruption");
            mock.set_default_response(if upstream_error {
                template.with_stream_error_after(
                    2,
                    CompletionError::CompletionError("upstream read failed".into()),
                )
            } else {
                template.with_disconnect_after(2)
            })
            .await;
            let mut request = json!({"model": model, "stream": true, "stream_options": options});
            if endpoint == "completions" {
                request["prompt"] = json!("Respond with a sentence.");
            } else {
                request["messages"] =
                    json!([{"role": "user", "content": "Respond with a sentence."}]);
            }
            let response = server
                .post(&format!("/v1/{endpoint}"))
                .add_header("Authorization", format!("Bearer {api_key}"))
                .json(&request)
                .await;
            response.assert_status_ok();
            let body = response.text();
            assert!(
                !body.contains("[DONE]"),
                "failed stream must not have a success terminator: {body}"
            );
            let events = json_events(&body);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.get("error").is_some())
                    .count(),
                1,
                "{body}"
            );
            assert!(
                events.last().unwrap().get("error").is_some(),
                "no content may follow the error: {body}"
            );
            assert!(
                events
                    .iter()
                    .all(
                        |event| event["choices"].as_array().is_none_or(|choices| choices
                            .iter()
                            .all(|choice| choice["finish_reason"].is_null()))
                    ),
                "{body}"
            );
            assert!(
                !events.iter().any(
                    |event| event["choices"].as_array().is_some_and(Vec::is_empty)
                        && event["usage"].is_object()
                ),
                "failed stream must not have a final usage summary: {body}"
            );
            let chat_id = events
                .iter()
                .find_map(|event| event["id"].as_str())
                .expect("partial output should reach the client before the error");
            let client = database.pool().get().await.unwrap();
            let count: i64 = client
                .query_one(
                    "SELECT COUNT(*) FROM chat_signatures WHERE chat_id = $1",
                    &[&chat_id],
                )
                .await
                .unwrap()
                .get(0);
            assert_eq!(count, 0, "failed stream must not be signed");
        }
    }
}

#[tokio::test]
async fn upstream_eof_before_the_first_event_returns_http_error() {
    let (server, _pool, mock, _db) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    mock.set_default_response(ResponseTemplate::new("unused").with_disconnect_after(0))
        .await;
    let response = server.post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "Hello"}], "stream": true})).await;
    assert!(
        response.status_code().is_server_error(),
        "{}",
        response.text()
    );
    assert!(response.json::<Value>()["error"].is_object());
}
