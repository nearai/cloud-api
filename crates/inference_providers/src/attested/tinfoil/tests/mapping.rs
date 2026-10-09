//! Availability mapping, wire mapping and SSE sanitisation tests.

use super::*;

// ---------------------------------------------------------------- availability

#[test]
fn status_mapping_matches_spec() {
    assert_eq!(map_upstream_status(503), UpstreamDisposition::Retryable503);
    assert_eq!(map_upstream_status(500), UpstreamDisposition::Retryable503);
    assert_eq!(
        map_upstream_status(429),
        UpstreamDisposition::Passthrough429
    );
    for s in [401, 402, 403] {
        assert_eq!(map_upstream_status(s), UpstreamDisposition::Retryable503);
    }
    // Not the caller's fault: fall through to the next provider.
    for st in [404, 408, 425] {
        assert_eq!(map_upstream_status(st), UpstreamDisposition::Retryable503);
    }
    assert_eq!(
        map_upstream_status(400),
        UpstreamDisposition::ReturnAs4xx(400)
    );
    assert_eq!(
        map_upstream_status(413),
        UpstreamDisposition::ReturnAs4xx(413)
    );
    assert_eq!(
        map_upstream_status(422),
        UpstreamDisposition::ReturnAs4xx(422)
    );
    // A redirect is never followed or trusted: unavailable.
    assert_eq!(map_upstream_status(302), UpstreamDisposition::Retryable503);
}

#[test]
fn unavailable_is_external_503() {
    match super::super::availability::unavailable("not_verified") {
        CompletionError::HttpError {
            status_code,
            is_external,
            message,
        } => {
            assert_eq!(status_code, 503);
            assert!(is_external);
            assert!(message.contains("not_verified"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn api_key_never_in_debug() {
    let c = Config::new("tk_secret_value".into(), 30);
    assert!(!format!("{c:?}").contains("tk_secret_value"));
}

#[test]
fn production_urls_are_constants() {
    let c = Config::new("k".into(), 30);
    assert_eq!(c.atc_url, super::super::ATC_URL);
    assert_eq!(super::super::BASE_URL, "https://inference.tinfoil.sh");
    assert_eq!(super::super::ATC_URL, "https://atc.tinfoil.sh/attestation");
    assert_eq!(super::super::PROXY_REREAD.as_secs(), 60);
    assert_eq!(super::super::ROUTER_REVERIFY.as_secs(), 300);
}

// ---------------------------------------------------------------- wire mapping

#[test]
fn nonstream_fixture_maps_model_and_reasoning() {
    let (bytes, resp) = wire::map_response(&fixture("chat_nonstream.json"), CANON).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["model"], CANON);
    assert_eq!(resp.model, CANON);
    let msg = &v["choices"][0]["message"];
    assert!(msg["reasoning_content"]
        .as_str()
        .unwrap()
        .starts_with("The user says"));
    assert!(msg.get("reasoning").is_none());
    assert_eq!(v["choices"][0]["finish_reason"], "length");
    let s = String::from_utf8(bytes).unwrap();
    for k in [
        "token_ids",
        "prompt_token_ids",
        "prompt_text",
        "stop_reason",
        "routed_experts",
        "kv_transfer_params",
        "prompt_logprobs",
    ] {
        assert!(!s.contains(k), "{k} leaked");
    }
    assert_eq!(resp.usage.prompt_tokens, 69);
}

#[test]
fn nonstream_glm_fixture_has_no_reasoning_and_keeps_usage_details() {
    let (bytes, resp) =
        wire::map_response(&fixture("chat_nonstream_glm53_high.json"), "zai/glm-5.3").unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v["choices"][0]["message"]
        .get("reasoning_content")
        .is_none());
    assert_eq!(resp.usage.reasoning_tokens(), Some(0));
}

fn parse_sse(fixture_name: &str) -> crate::StreamingResult {
    let bytes = bytes::Bytes::from(fixture(fixture_name));
    let s = futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes)]);
    Box::pin(crate::sse_parser::new_external_sse_parser(s, true))
}

async fn run_stream(name: &str, include_usage: bool) -> Vec<crate::SSEEvent> {
    let mapped = wire::map_stream(parse_sse(name), CANON.to_string(), include_usage);
    mapped.map(|e| e.unwrap()).collect().await
}

#[tokio::test]
async fn stream_fixture_maps_deltas_and_usage() {
    let evs = run_stream("chat_stream.sse", true).await;
    let mut reasoning = String::new();
    let mut usage_chunks = 0;
    for ev in &evs {
        let raw = String::from_utf8_lossy(&ev.raw_bytes).to_string();
        for k in [
            "\"p\"",
            "token_ids",
            "prompt_text",
            "stop_reason",
            "\"reasoning\"",
        ] {
            assert!(!raw.contains(k), "{k} leaked: {raw}");
        }
        if let Some(v) = client_json(ev) {
            assert_eq!(v["model"], CANON);
            if let Some(r) = v["choices"][0]["delta"]["reasoning_content"].as_str() {
                reasoning.push_str(r);
            }
            if v.get("usage").is_some() {
                usage_chunks += 1;
                assert!(
                    v["choices"].as_array().unwrap().is_empty(),
                    "usage only on final chunk"
                );
                assert_eq!(v["usage"]["completion_tokens"], 16);
            }
        }
    }
    assert!(reasoning.starts_with("The user says"));
    assert_eq!(usage_chunks, 1);
    assert!(evs.iter().any(|e| e.is_done_marker()));
}

#[tokio::test]
async fn stream_without_client_usage_hides_usage_but_keeps_it_for_billing() {
    let evs = run_stream("chat_stream.sse", false).await;
    let mut billed = None;
    for ev in &evs {
        assert!(!String::from_utf8_lossy(&ev.raw_bytes).contains("usage"));
        if let Some(crate::StreamChunk::Chat(c)) = &ev.chunk {
            if let Some(u) = &c.usage {
                billed = Some(u.completion_tokens);
            }
        }
    }
    assert_eq!(billed, Some(16));
}

#[tokio::test]
async fn stream_no_usage_fixture_strips_cumulative_per_chunk_usage() {
    let evs = run_stream("chat_stream_no_usage.sse", false).await;
    for ev in &evs {
        assert!(!String::from_utf8_lossy(&ev.raw_bytes).contains("usage"));
    }
    // Billing still sees the last cumulative usage.
    let last = evs
        .iter()
        .filter_map(|e| match &e.chunk {
            Some(crate::StreamChunk::Chat(c)) => c.usage.clone(),
            _ => None,
        })
        .next_back()
        .unwrap();
    assert_eq!(last.completion_tokens, 16);
}

async fn map_raw_sse(raw: &str) -> Vec<Result<crate::SSEEvent, crate::CompletionError>> {
    let s = futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(
        raw.to_string(),
    ))]);
    let parsed: crate::StreamingResult =
        Box::pin(crate::sse_parser::new_external_sse_parser(s, true));
    wire::map_stream(parsed, CANON.to_string(), false)
        .collect()
        .await
}

const LEAKY: &str = r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"upstream-slug","prompt_text":"SECRET","prompt_token_ids":[1,2],"choices":[{"index":0,"token_ids":[9],"delta":{"content":"hi","token_ids":[9]}}]}"#;

#[tokio::test]
async fn data_frame_without_space_is_sanitized_not_passed_through() {
    let out = map_raw_sse(&format!("data:{LEAKY}\n\ndata: [DONE]\n\n")).await;
    let mut saw_chunk = false;
    for ev in out {
        let ev = ev.unwrap();
        let raw = String::from_utf8_lossy(&ev.raw_bytes).to_string();
        for k in ["prompt_text", "SECRET", "token_ids", "upstream-slug"] {
            assert!(!raw.contains(k), "{k} leaked: {raw}");
        }
        if let Some(v) = client_json(&ev) {
            assert_eq!(v["model"], CANON);
            saw_chunk = true;
        }
    }
    assert!(saw_chunk);
}

#[tokio::test]
async fn data_done_without_space_still_terminates() {
    let out = map_raw_sse("data:[DONE]\n\n").await;
    assert!(out.into_iter().any(|e| e.unwrap().is_done_marker()));
}

#[tokio::test]
async fn unknown_lines_are_never_forwarded() {
    for line in [
        format!("{LEAKY}\n\n"),
        format!("Event: {LEAKY}\n\n"),
        format!("bare text {LEAKY}\n\n"),
    ] {
        let out = map_raw_sse(&line).await;
        assert!(out.iter().any(|r| r.is_err()), "not rejected: {line}");
        for ev in out.iter().flatten() {
            let raw = String::from_utf8_lossy(&ev.raw_bytes);
            assert!(!raw.contains("SECRET"), "unknown line forwarded: {raw}");
        }
    }
}

#[tokio::test]
async fn blank_and_comment_lines_still_pass_through() {
    let out = map_raw_sse(": keepalive\n\n").await;
    assert!(!out.is_empty() && out.iter().all(|r| r.is_ok()));
}

#[tokio::test]
async fn comment_with_embedded_cr_is_rejected() {
    for raw in [
        format!(": x\rdata:{LEAKY}\n\n"),
        ": x\r\rdata: y\n\n".to_string(),
        format!("data: [DONE]\rdata:{LEAKY}\n\n"),
    ] {
        let out = map_raw_sse(&raw).await;
        assert!(out.iter().any(|r| r.is_err()), "CR line not rejected");
        for ev in out.iter().flatten() {
            let b = String::from_utf8_lossy(&ev.raw_bytes);
            assert!(!b.contains("SECRET") && !b.contains('\r'), "forwarded: {b}");
        }
    }
}

#[tokio::test]
async fn comment_with_crlf_terminator_still_passes() {
    let out = map_raw_sse(": keepalive\r\n\r\n").await;
    assert!(!out.is_empty() && out.iter().all(|r| r.is_ok()));
}

#[tokio::test]
async fn standard_fields_map_to_fixed_comment() {
    for raw in [
        "event: SECRETEVT\n\n",
        "id: 1SECRETID\n\n",
        "retry: 5000\n\n",
        "event:SECRETEVT\n\n",
        "event\n\n",
    ] {
        let out = map_raw_sse(raw).await;
        assert!(out.iter().all(|r| r.is_ok()), "stream aborted for {raw:?}");
        let first = out[0].as_ref().unwrap();
        assert_eq!(&first.raw_bytes[..], b":\n", "{raw:?}");
        assert!(first.chunk.is_none());
        for ev in out.iter().flatten() {
            let b = String::from_utf8_lossy(&ev.raw_bytes);
            assert!(!b.contains("SECRET") && !b.contains("5000"), "leaked: {b}");
        }
    }
}

const NESTED_ARGS: &str = r#"{"q":"a \"b\" c"}"#;

fn nested_leaky(inner: &str, object: &str) -> String {
    serde_json::json!({
        "id": "c", "object": object, "created": 0, "model": "upstream-slug",
        "choices": [{"index": 0, "finish_reason": null,
            "logprobs": {"content": [{"token": "a", "logprob": -0.5, "bytes": [97],
                "prompt_text": "SECRET", "top_logprobs": [{"token": "a", "logprob": -0.5, "bytes": [97], "prompt_text": "SECRET"}]}]},
            inner: {"role": "assistant", "content": "hi", "tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                "prompt_text": "SECRET",
                "function": {"name": "f", "arguments": NESTED_ARGS, "prompt_text": "SECRET"}}]}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2,
            "prompt_tokens_details": {"cached_tokens": 0, "prompt_text": "SECRET"}}
    })
    .to_string()
}

fn assert_nested_clean(raw: &str, v: &serde_json::Value, inner: &str) {
    assert!(
        !raw.contains("SECRET") && !raw.contains("prompt_text"),
        "{raw}"
    );
    let tc = &v["choices"][0][inner]["tool_calls"][0];
    assert_eq!(tc["id"], "call_1");
    assert_eq!(tc["index"], 0);
    assert_eq!(tc["function"]["arguments"].as_str().unwrap(), NESTED_ARGS);
    assert_eq!(v["choices"][0]["logprobs"]["content"][0]["bytes"][0], 97);
}

#[test]
fn nested_prompt_text_is_stripped_from_non_stream_responses() {
    let body = nested_leaky("message", "chat.completion");
    let (raw, _) = wire::map_response(body.as_bytes(), CANON).unwrap();
    let raw = String::from_utf8(raw).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_nested_clean(&raw, &v, "message");
}

#[tokio::test]
async fn nested_prompt_text_is_stripped_from_stream_chunks() {
    let body = nested_leaky("delta", "chat.completion.chunk");
    let out = map_raw_sse(&format!("data: {body}\n\ndata: [DONE]\n\n")).await;
    let mut seen = false;
    for ev in out {
        let ev = ev.unwrap();
        let raw = String::from_utf8_lossy(&ev.raw_bytes).to_string();
        if let Some(v) = client_json(&ev) {
            assert_nested_clean(&raw, &v, "delta");
            seen = true;
        }
    }
    assert!(seen);
}
