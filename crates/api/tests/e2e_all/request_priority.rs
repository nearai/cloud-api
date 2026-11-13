// E2E tests for the organization's scheduler priority on inference requests.
// The priority (`organizations.request_priority`) is written by the admin API and is
// seeded here; the kept inference routes must forward the organization's value and
// ignore whatever the client sends.

use crate::common::*;
use serde_json::{json, Value};

const ENDPOINTS: [&str; 3] = ["chat/completions", "completions", "responses"];

fn request_body(endpoint: &str, stream: bool) -> Value {
    // Client-sent values must never reach the provider as the priority.
    let mut body = json!({"model": E2E_QWEN_MODEL_NAME, "stream": stream,
        "priority": 999, "request_priority": 999, "max_tokens": 32});
    match endpoint {
        "chat/completions" => {
            body["messages"] = json!([{"role": "user", "content": "synthetic test"}])
        }
        "completions" => body["prompt"] = json!("synthetic test"),
        _ => {
            body["input"] = json!("synthetic test");
            body.as_object_mut().unwrap().remove("max_tokens");
        }
    }
    body
}

#[tokio::test]
async fn priority_flows_through_chat_text_and_responses_and_client_values_are_ignored() {
    let (server, _, mock, _) = setup_test_server_with_pool().await;
    for priority in [0, -2, 1000] {
        // One organization per priority: the key's organization is cached after first use.
        let org = setup_org_with_credits(&server, 100_000_000_000).await;
        seeding::seed_request_priority(&org.id, priority).await;
        let key = get_api_key_for_org(&server, org.id.clone()).await;
        for endpoint in ENDPOINTS {
            for stream in [false, true] {
                let response = server
                    .post(&format!("/v1/{endpoint}"))
                    .add_header("Authorization", format!("Bearer {key}"))
                    .add_header("X-NearAI-Priority", "-1000")
                    .json(&request_body(endpoint, stream))
                    .await;
                assert_eq!(
                    response.status_code(),
                    200,
                    "{endpoint}: {}",
                    response.text()
                );
                let params = mock.last_chat_params().await.unwrap();
                assert_eq!(
                    params.request_priority, priority,
                    "{endpoint} stream={stream}"
                );
                assert!(!params.extra.contains_key("priority"));
                assert!(!params.extra.contains_key("request_priority"));
                if let Some(original) = &params.original_request {
                    assert!(original.get("priority").is_none());
                    assert!(original.get("request_priority").is_none());
                }
                if stream {
                    assert!(response.text().contains(if endpoint == "responses" {
                        "response.completed"
                    } else {
                        "[DONE]"
                    }));
                }
            }
        }
    }
}

#[tokio::test]
async fn concurrent_requests_of_different_priority_are_not_mixed_up() {
    let (server, _, mock, _) = setup_test_server_with_pool().await;
    let org = setup_org_with_credits(&server, 100_000_000_000).await;
    seeding::seed_request_priority(&org.id, -2).await;
    let key = get_api_key_for_org(&server, org.id.clone()).await;
    let other = setup_org_with_credits(&server, 100_000_000_000).await;
    let other_key = get_api_key_for_org(&server, other.id.clone()).await;

    let before = mock.chat_request_priorities().await.len();
    let body = json!({"model": E2E_QWEN_MODEL_NAME,
        "messages": [{"role": "user", "content": "synthetic concurrent test"}]});
    let first = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&body);
    let second = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {other_key}"))
        .json(&body);
    let (first, second) = tokio::join!(first, second);
    first.assert_status_ok();
    second.assert_status_ok();
    let mut seen = mock.chat_request_priorities().await[before..].to_vec();
    seen.sort();
    assert_eq!(seen, vec![-2, 0]);
}
