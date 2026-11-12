//! Native hosted search is executed upstream, with raw protocol and replay preserved.

use super::responses_hosted_search::assert_no_history;
use crate::common::*;
use bytes::Bytes;
use inference_providers::{mock::MockProvider, responses_raw::ResponsesRawResponse};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn native_web_search_json_preserves_protocol_and_full_output_replay() {
    native_search_flow(false).await;
}

#[tokio::test]
async fn native_web_search_sse_preserves_protocol_and_full_output_replay() {
    native_search_flow(true).await;
}

async fn native_search_flow(stream: bool) {
    let (server, pool, _, database) = setup_test_server_with_pool().await;
    let model = format!("openai/native-search-{}", uuid::Uuid::new_v4());
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let upstream = Arc::new(Mutex::new(Vec::<(Value, String)>::new()));
    let calls = captured.clone();
    let replies = upstream.clone();
    let provider = Arc::new(MockProvider::new_accept_all().with_responses_handler(move |body: Value| {
        calls.lock().unwrap().push(body.clone());
        let replay = body["tool_choice"] == "none";
        let search = json!({
            "id":"ws_native", "type":"web_search_call", "status":"completed",
            "action":{"type":"search","query":"native search documentation",
                "sources":[{"type":"url","url":"https://example.com/native-search"}]}
        });
        let message = json!({
            "id":"msg_native", "type":"message", "role":"assistant", "status":"completed",
            "content":[{"type":"output_text","text":"Native search documentation.",
                "annotations":[{"type":"url_citation","start_index":0,"end_index":27,
                    "url":"https://example.com/native-search","title":"Native search documentation"}]}]
        });
        let output = if replay { vec![message] } else { vec![
            json!({"type":"reasoning","id":"rs_native","summary":[],"encrypted_content":"opaque-native-reasoning"}),
            search.clone(), message,
        ] };
        let response = json!({
            "id":format!("resp_{}", uuid::Uuid::new_v4()), "object":"response", "status":"completed",
            "model":body["model"], "output":output, "store":false, "service_tier":"default",
            "usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}
        });
        let bytes = if body["stream"] == true {
            let mut events = vec![json!({"type":"response.created","response":{
                "id":response["id"],"object":"response","status":"in_progress","output":[]
            }})];
            if !replay {
                for kind in ["in_progress", "searching", "completed"] {
                    events.push(json!({"type":format!("response.web_search_call.{kind}"),
                        "item_id":"ws_native","output_index":1}));
                }
                events.push(json!({"type":"response.output_item.done","output_index":1,"item":search}));
            }
            events.push(json!({"type":"response.completed","response":response}));
            events.into_iter().enumerate().map(|(index, mut event)| {
                event["sequence_number"] = json!(index);
                format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap())
            }).collect::<String>()
        } else {
            // Deliberate whitespace detects accidental JSON reserialization.
            serde_json::to_string_pretty(&response).unwrap()
        };
        replies.lock().unwrap().push((response, bytes.clone()));
        let bytes = Bytes::from(bytes);
        let split = bytes.len() / 2;
        ResponsesRawResponse {
            status: axum::http::StatusCode::OK,
            headers: [(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static(
                if body["stream"] == true { "text/event-stream" } else { "application/json" }
            ))].into_iter().collect(),
            body: Box::pin(futures_util::stream::iter(vec![Ok(bytes.slice(..split)), Ok(bytes.slice(split..))])),
        }
    }));
    let mut batch = api::models::BatchUpdateModelApiRequest::new();
    batch.insert(model.clone(), serde_json::from_value(json!({
        "inputCostPerToken":{"amount":1000,"currency":"USD"},
        "outputCostPerToken":{"amount":2000,"currency":"USD"},
        "modelDisplayName":"Native search fixture","modelDescription":"Native hosted search test",
        "contextLength":10000,"maxOutputLength":1000,"isActive":true,"ownedBy":"openai",
        "verifiable":false,"inputModalities":["text"],"outputModalities":["text"]
    })).unwrap());
    seed_models(&server, batch, get_session_id()).await;
    pool.register_provider(model.clone(), provider.clone())
        .await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let key = get_api_key_for_org(&server, org.id).await;
    let mut request = json!({
        "model":model,"store":false,"stream":stream,
        "input":[{"role":"user","content":"Find native search documentation."}],
        "tools":[{"type":"web_search","search_context_size":"low",
            "filters":{"allowed_domains":["example.com"]},"external_web_access":true,
            "user_location":{"type":"approximate","country":"US","city":"Seattle"}}],
        "tool_choice":{"type":"web_search"},
        "include":["web_search_call.action.sources","reasoning.encrypted_content"]
    });

    for turn in 0..2 {
        let result = server
            .post("/v1/responses")
            .add_header("Authorization", format!("Bearer {key}"))
            .json(&request)
            .await;
        assert_eq!(result.status_code(), 200, "{}", result.text());
        assert_eq!(result.headers()["cache-control"], "no-store");
        let (response, raw) = upstream.lock().unwrap()[turn].clone();
        assert_eq!(
            result.text(),
            raw,
            "native JSON/SSE must be relayed byte-for-byte"
        );
        {
            let calls = captured.lock().unwrap();
            assert_eq!(
                calls.len(),
                turn + 1,
                "one upstream Responses request per turn"
            );
            for field in [
                "model",
                "store",
                "stream",
                "input",
                "tools",
                "tool_choice",
                "include",
            ] {
                assert_eq!(calls[turn][field], request[field], "native {field} changed");
            }
            assert!(calls[turn].get("previous_response_id").is_none());
            assert!(calls[turn].get("conversation").is_none());
        }
        assert_eq!(provider.chat_completion_call_count(), 0);
        assert_no_history(&database, &response).await;
        if turn == 0 {
            let input = request["input"].as_array_mut().unwrap();
            input.extend(response["output"].as_array().unwrap().iter().cloned());
            input.push(
                json!({"role":"user","content":"Repeat the cited URL without searching again."}),
            );
            request["tool_choice"] = json!("none");
        }
    }
}
