use super::*;
use crate::routes::anthropic::tests::{
    billing_context, wait_for_recorded_usage, RecordingUsageService,
};
use crate::routes::anthropic::{build_upstream_response, NativeUsageStream, SseUsageParser};
use axum::{body::to_bytes, http::StatusCode};
use inference_providers::{AnthropicRawEndpoint, AnthropicRawResponse, ProviderFilter};
use services::attestation::{
    ita::{ItaTokenQuery, ItaTokenResponse},
    models::AttestationReport,
    AttestationError, SignatureLookupResult,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};
use std::time::Duration;
use tokio::sync::{Notify, Semaphore};

const REQUEST: &[u8] = b"{ \"model\": \"anthropic/alias\", \"stream\": true }";
const START: &[u8] = b"event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_receipt\",\"usage\":{\"input_tokens\":3}}}\r\n\r\n";
const STOP: &[u8] = b"event: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n";

#[derive(Default)]
struct ReceiptStore {
    records: Mutex<Vec<(String, String, String)>>,
    attempts: AtomicUsize,
    started: Notify,
    gate: Option<Semaphore>,
    fail: bool,
}

#[async_trait::async_trait]
impl AttestationServiceTrait for ReceiptStore {
    async fn store_chat_signature(
        &self,
        id: &str,
        request: String,
        response: String,
    ) -> Result<(), AttestationError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        if let Some(gate) = &self.gate {
            let _permit = gate.acquire().await.unwrap();
        }
        if self.fail {
            return Err(AttestationError::InternalError(
                "fixture storage failure".into(),
            ));
        }
        self.records
            .lock()
            .unwrap()
            .push((id.into(), request, response));
        Ok(())
    }

    async fn get_chat_signature(
        &self,
        _: &str,
        _: Option<String>,
    ) -> Result<SignatureLookupResult, AttestationError> {
        unimplemented!()
    }
    async fn store_chat_signature_from_provider(&self, _: &str) -> Result<(), AttestationError> {
        unimplemented!()
    }
    async fn store_response_signature(
        &self,
        _: &str,
        _: String,
        _: String,
    ) -> Result<(), AttestationError> {
        unimplemented!()
    }
    async fn get_attestation_report(
        &self,
        _: Option<String>,
        _: Option<String>,
        _: Option<String>,
        _: Option<String>,
        _: bool,
        _: Option<ProviderFilter>,
    ) -> Result<AttestationReport, AttestationError> {
        unimplemented!()
    }
    async fn get_ita_attestation_token(
        &self,
        _: ItaTokenQuery,
    ) -> Result<ItaTokenResponse, AttestationError> {
        unimplemented!()
    }
    async fn verify_vpc_signature(&self, _: i64, _: String) -> Result<bool, AttestationError> {
        unimplemented!()
    }
}

fn raw_stream(items: Vec<Result<Bytes, AnthropicRawError>>) -> AnthropicRawBody {
    Box::pin(futures_util::stream::iter(items))
}

async fn collect(mut stream: AnthropicRawBody) -> Vec<u8> {
    let mut body = Vec::new();
    while let Some(bytes) = stream.next().await {
        body.extend_from_slice(&bytes.unwrap());
    }
    body
}

fn receipt_context(store: &Arc<ReceiptStore>) -> MessageSigningContext {
    MessageSigningContext::new(store.clone(), body_hash(REQUEST))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unusable_start_ids_stream_immediately_and_bill_without_receipts() {
    use futures_util::FutureExt;
    use serde_json::json;

    for id in [
        Some(json!("x".repeat(256))),
        Some(json!("msg_\ninvalid")),
        Some(json!("")),
        Some(json!(42)),
        None,
    ] {
        let store = Arc::new(ReceiptStore::default());
        let usage = Arc::new(RecordingUsageService::default());
        let mut message = json!({"usage":{"input_tokens":3}});
        if let Some(id) = id {
            message["id"] = id;
        }
        let start = Bytes::from(format!(
            "event: message_start\ndata: {}\n\n",
            json!({"type":"message_start","message":message})
        ));
        let tail = Bytes::from(
            [
                b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n"
                    .as_slice(),
                STOP,
            ]
            .concat(),
        );
        let release = Arc::new(Notify::new());
        let upstream_release = release.clone();
        let upstream_tail = tail.clone();
        let inner =
            raw_stream(vec![Ok(start.clone())]).chain(futures_util::stream::once(async move {
                upstream_release.notified().await;
                Ok(upstream_tail)
            }));
        // The tail is deliberately unavailable. The response must be ready
        // immediately, without waiting for the 30-second peek timeout.
        let response = build_upstream_response(
            AnthropicRawResponse {
                status: StatusCode::OK,
                headers: Default::default(),
                body: Box::pin(inner),
            },
            AnthropicRawEndpoint::Messages,
            true,
            Some(billing_context(usage.clone())),
            None,
            None,
            Some(receipt_context(&store)),
        )
        .now_or_never()
        .expect("an observed invalid/missing ID must end the peek");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key("inference-id"));
        let mut body = response.into_body().into_data_stream();
        assert_eq!(body.next().now_or_never().unwrap().unwrap().unwrap(), start);
        release.notify_one();
        let mut returned_tail = Vec::new();
        while let Some(bytes) = body.next().await {
            returned_tail.extend_from_slice(&bytes.unwrap());
        }
        assert_eq!(returned_tail, tail);
        drop(body);
        let recorded = wait_for_recorded_usage(&usage).await;
        assert_eq!(recorded.provider_request_id, None);
        assert_eq!(recorded.input_tokens, 3);
        assert_eq!(recorded.output_tokens, 9);
        assert_eq!(
            recorded.stop_reason,
            Some(services::usage::StopReason::Completed)
        );
        assert!(usage.records.lock().unwrap().is_empty());
        assert_eq!(store.attempts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn json_billing_errors_and_task_panics_keep_distinct_diagnostics() {
    #[derive(Clone)]
    struct LogWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    for panic_on_record in [false, true] {
        let store = Arc::new(ReceiptStore::default());
        let usage = Arc::new(RecordingUsageService {
            panic_on_record,
            ..Default::default()
        });
        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter(logs.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        // This test uses the current-thread runtime, including across await.
        let _guard = tracing::subscriber::set_default(subscriber);
        let response = build_upstream_response(
            AnthropicRawResponse {
                status: StatusCode::OK,
                headers: Default::default(),
                body: raw_stream(vec![Ok(Bytes::from_static(
                    br#"{"id":"msg_billing_error","usage":{"input_tokens":3,"output_tokens":9}}"#,
                ))]),
            },
            AnthropicRawEndpoint::Messages,
            false,
            Some(billing_context(usage)),
            None,
            None,
            Some(receipt_context(&store)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"]["message"], "Failed to record usage");
        assert_eq!(store.attempts.load(Ordering::SeqCst), 0);
        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        if panic_on_record {
            assert!(logs.contains("Native Anthropic usage finalization task failed"));
            assert!(logs.contains("task_panicked=true"));
            assert!(logs.contains("task_cancelled=false"));
            assert!(!logs.contains("fixture usage panic"));
        } else {
            assert!(logs.contains("Failed to record native Anthropic usage"));
            assert!(logs.contains("Internal error: recorded"));
        }
    }
}

#[tokio::test]
async fn raw_sse_hash_includes_prelude_tools_and_terminal_for_arbitrary_chunks() {
    let raw = [START,
        b": heartbeat\r\n\r\nevent: ping\ndata: {\"type\":\"ping\"}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\\\"东京\\\"}\"}}\n\n".as_bytes(),
        b"event: future_event\ndata: {\"type\":\"future_event\"}\n\n",
        STOP].concat();
    for size in [1, 2, 7, 53, raw.len()] {
        let store = Arc::new(ReceiptStore::default());
        let raw_body = raw_stream(
            raw.chunks(size)
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect(),
        );
        let mut usage_stream = NativeUsageStream {
            inner: raw_body,
            parser: SseUsageParser::default(),
            prelude: Default::default(),
            inner_done: false,
            billing: None,
            concurrent_slot: None,
            runtime_handle: tokio::runtime::Handle::current(),
        };
        usage_stream.peek_message_id().await;
        assert_eq!(
            usage_stream.parser.usage.provider_request_id.as_deref(),
            Some("msg_receipt")
        );
        let bytes = collect(signed_stream(
            Box::pin(usage_stream),
            receipt_context(&store),
        ))
        .await;
        assert_eq!(bytes, raw, "chunk size {size}");
        assert_eq!(
            *store.records.lock().unwrap(),
            vec![("msg_receipt".into(), body_hash(REQUEST), body_hash(&raw))]
        );
        assert_eq!(store.attempts.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn incomplete_errored_missing_or_changed_ids_never_store_receipts() {
    let cases = vec![
        START.to_vec(),
        [
            START,
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n",
        ]
        .concat(),
        [
            START,
            b"data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n\n",
            STOP,
        ]
        .concat(),
        [
            b"data: {\"type\":\"message_start\",\"message\":{}}\n\n".as_slice(),
            STOP,
        ]
        .concat(),
        [
            START,
            b"data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_other\"}}\n\n",
            STOP,
        ]
        .concat(),
        [START, b"event: message_stop\ndata: {\"type\":\"ping\"}\n\n"].concat(),
    ];
    for raw in cases {
        let store = Arc::new(ReceiptStore::default());
        let stream = signed_stream(
            raw_stream(vec![Ok(Bytes::from(raw.clone()))]),
            receipt_context(&store),
        );
        assert_eq!(collect(stream).await, raw);
        assert_eq!(store.attempts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn oversized_event_passes_through_without_unbounded_buffer_or_receipt() {
    let raw = [START, &vec![b'x'; MAX_EVENT_BYTES + 100], b"\n\n", STOP].concat();
    let store = Arc::new(ReceiptStore::default());
    let stream = signed_stream(
        raw_stream(vec![Ok(Bytes::from(raw.clone()))]),
        receipt_context(&store),
    );
    assert_eq!(collect(stream).await, raw);
    assert_eq!(store.attempts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn transport_error_preserves_buffered_bytes_without_receipt() {
    let store = Arc::new(ReceiptStore::default());
    let mut stream = signed_stream(
        raw_stream(vec![
            Ok(Bytes::from_static(START)),
            Ok(Bytes::from_static(b"data: partial")),
            Err(AnthropicRawError::Transport("fixture".into())),
        ]),
        receipt_context(&store),
    );
    assert_eq!(stream.next().await.unwrap().unwrap(), START);
    assert_eq!(
        stream.next().await.unwrap().unwrap(),
        b"data: partial".as_slice()
    );
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
    assert_eq!(store.attempts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn terminal_waits_for_receipt_and_does_not_wait_for_upstream_eof() {
    let store = Arc::new(ReceiptStore {
        gate: Some(Semaphore::new(0)),
        ..Default::default()
    });
    let raw = [START, STOP].concat();
    let inner = Box::pin(
        futures_util::stream::iter(vec![Ok(Bytes::from(raw.clone()))])
            .chain(futures_util::stream::pending()),
    );
    let mut stream = signed_stream(inner, receipt_context(&store));
    assert_eq!(stream.next().await.unwrap().unwrap(), START);
    let task = tokio::spawn(async move { collect(stream).await });
    tokio::time::timeout(Duration::from_secs(1), store.started.notified())
        .await
        .unwrap();
    assert!(!task.is_finished(), "terminal event must wait for storage");
    store.gate.as_ref().unwrap().add_permits(1);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap(),
        STOP
    );
    assert_eq!(store.records.lock().unwrap()[0].2, body_hash(&raw));
}

#[tokio::test]
async fn cancellation_during_finalization_does_not_cancel_receipt() {
    let store = Arc::new(ReceiptStore {
        gate: Some(Semaphore::new(0)),
        ..Default::default()
    });
    let raw = [START, STOP].concat();
    let stream = signed_stream(
        raw_stream(vec![Ok(Bytes::from(raw))]),
        receipt_context(&store),
    );
    let task = tokio::spawn(collect(stream));
    tokio::time::timeout(Duration::from_secs(1), store.started.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    store.gate.as_ref().unwrap().add_permits(1);
    tokio::time::timeout(Duration::from_secs(1), async {
        while store.records.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(store.attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_or_timed_out_storage_preserves_response_without_retry() {
    for hanging in [false, true] {
        let store = Arc::new(ReceiptStore {
            fail: !hanging,
            gate: hanging.then(|| Semaphore::new(0)),
            ..Default::default()
        });
        let raw = [START, STOP].concat();
        let stream = signed_stream(
            raw_stream(vec![Ok(Bytes::from(raw.clone()))]),
            receipt_context(&store),
        );
        assert_eq!(
            tokio::time::timeout(
                SIGNATURE_STORE_TIMEOUT + Duration::from_secs(1),
                collect(stream)
            )
            .await
            .unwrap(),
            raw
        );
        assert_eq!(store.attempts.load(Ordering::SeqCst), 1);
        assert!(store.records.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn json_signs_verbatim_success_only_and_keeps_billing_id() {
    let raw = b"{ \"id\": \"msg_receipt\", \"usage\": {\"input_tokens\": 3, \"output_tokens\": 5}, \"content\": [] }";
    for (endpoint, status, signed) in [
        (AnthropicRawEndpoint::Messages, StatusCode::OK, true),
        (
            AnthropicRawEndpoint::Messages,
            StatusCode::BAD_REQUEST,
            false,
        ),
        (AnthropicRawEndpoint::CountTokens, StatusCode::OK, false),
    ] {
        let store = Arc::new(ReceiptStore::default());
        let response = build_upstream_response(
            AnthropicRawResponse {
                status,
                headers: Default::default(),
                body: Box::pin(futures_util::stream::iter(vec![Ok(Bytes::from_static(
                    raw,
                ))])),
            },
            endpoint,
            false,
            None,
            None,
            None,
            Some(receipt_context(&store)),
        )
        .await;
        if signed {
            assert_eq!(
                response.headers()["inference-id"],
                services::completions::hash_inference_id_to_uuid("msg_receipt").to_string()
            );
        }
        assert_eq!(
            to_bytes(response.into_body(), 4096).await.unwrap(),
            raw.as_slice()
        );
        assert_eq!(store.attempts.load(Ordering::SeqCst), usize::from(signed));
        if signed {
            assert_eq!(store.records.lock().unwrap()[0].2, body_hash(raw));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiline_events_keep_signature_and_billing_ids_in_sync_and_ignore_trailing_bytes() {
    let start = b"event: message_start\ndata: {\"type\":\"message_start\",\ndata: \"message\":{\"id\":\"msg_multiline\",\"usage\":{\"input_tokens\":4}}}\n\n";
    let delta = b"event: message_delta\ndata: {\"type\":\"message_delta\",\ndata: \"usage\":{\"output_tokens\":9}}\n\n";
    let expected = [start.as_slice(), delta.as_slice(), STOP].concat();
    let raw = [expected.as_slice(), START].concat();
    let store = Arc::new(ReceiptStore::default());
    let usage = Arc::new(RecordingUsageService::default());
    let response = build_upstream_response(
        AnthropicRawResponse {
            status: StatusCode::OK,
            headers: Default::default(),
            body: raw_stream(vec![Ok(Bytes::from(raw))]),
        },
        AnthropicRawEndpoint::Messages,
        true,
        Some(billing_context(usage.clone())),
        None,
        None,
        Some(receipt_context(&store)),
    )
    .await;
    let inference_id = services::completions::hash_inference_id_to_uuid("msg_multiline");
    assert_eq!(response.headers()["inference-id"], inference_id.to_string());
    assert_eq!(
        to_bytes(response.into_body(), 4096).await.unwrap(),
        expected
    );
    let recorded = wait_for_recorded_usage(&usage).await;
    assert_eq!(recorded.inference_id, Some(inference_id));
    assert_eq!(recorded.input_tokens, 4);
    assert_eq!(recorded.output_tokens, 9);
    assert_eq!(
        recorded.stop_reason,
        Some(services::usage::StopReason::Completed)
    );
    assert!(usage.records.lock().unwrap().is_empty());
    assert_eq!(
        *store.records.lock().unwrap(),
        vec![(
            "msg_multiline".into(),
            body_hash(REQUEST),
            body_hash(&expected)
        )]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signed_stream_disconnect_and_error_still_bill_without_creating_receipts() {
    for disconnect in [true, false] {
        let store = Arc::new(ReceiptStore::default());
        let usage = Arc::new(RecordingUsageService::default());
        let response = build_upstream_response(
            AnthropicRawResponse {
                status: StatusCode::OK,
                headers: Default::default(),
                body: raw_stream(vec![
                    Ok(Bytes::from_static(START)),
                    if disconnect {
                        Ok(Bytes::from_static(STOP))
                    } else {
                        Err(AnthropicRawError::Transport("fixture".into()))
                    },
                ]),
            },
            AnthropicRawEndpoint::Messages,
            true,
            Some(billing_context(usage.clone())),
            None,
            None,
            Some(receipt_context(&store)),
        )
        .await;
        let mut body = response.into_body().into_data_stream();
        assert_eq!(body.next().await.unwrap().unwrap(), START);
        if !disconnect {
            assert!(body.next().await.unwrap().is_err());
        }
        drop(body);
        let recorded = wait_for_recorded_usage(&usage).await;
        assert_eq!(
            recorded.inference_id,
            Some(services::completions::hash_inference_id_to_uuid(
                "msg_receipt"
            ))
        );
        assert_eq!(recorded.input_tokens, 3);
        assert_eq!(
            recorded.stop_reason,
            Some(if disconnect {
                services::usage::StopReason::ClientDisconnect
            } else {
                services::usage::StopReason::ProviderError
            })
        );
        assert!(usage.records.lock().unwrap().is_empty());
        assert_eq!(store.attempts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_preserves_usage_already_peeked_in_a_partial_event() {
    let store = Arc::new(ReceiptStore::default());
    let usage = Arc::new(RecordingUsageService::default());
    // The provider delivered usage in the same chunk as message_start, but
    // the last event is incomplete and the client consumes only the start.
    let raw = [
        START,
        b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}",
    ]
    .concat();
    let response = build_upstream_response(
        AnthropicRawResponse {
            status: StatusCode::OK,
            headers: Default::default(),
            body: Box::pin(
                raw_stream(vec![Ok(Bytes::from(raw))]).chain(futures_util::stream::pending()),
            ),
        },
        AnthropicRawEndpoint::Messages,
        true,
        Some(billing_context(usage.clone())),
        None,
        None,
        Some(receipt_context(&store)),
    )
    .await;
    let mut body = response.into_body().into_data_stream();
    assert_eq!(body.next().await.unwrap().unwrap(), START);
    drop(body);
    let recorded = wait_for_recorded_usage(&usage).await;
    assert_eq!(recorded.input_tokens, 3);
    assert_eq!(recorded.output_tokens, 9);
    assert_eq!(
        recorded.stop_reason,
        Some(services::usage::StopReason::ClientDisconnect)
    );
    assert_eq!(store.attempts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn json_missing_or_invalid_id_never_invents_a_receipt_id() {
    for id in [
        serde_json::Value::Null,
        serde_json::json!(""),
        serde_json::json!("x".repeat(256)),
        serde_json::json!("msg_\n"),
    ] {
        let raw = serde_json::to_vec(
            &serde_json::json!({"id":id,"usage":{"input_tokens":1,"output_tokens":2}}),
        )
        .unwrap();
        let store = Arc::new(ReceiptStore::default());
        let response = build_upstream_response(
            AnthropicRawResponse {
                status: StatusCode::OK,
                headers: Default::default(),
                body: raw_stream(vec![Ok(Bytes::from(raw.clone()))]),
            },
            AnthropicRawEndpoint::Messages,
            false,
            None,
            None,
            None,
            Some(receipt_context(&store)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key("inference-id"));
        assert_eq!(to_bytes(response.into_body(), 4096).await.unwrap(), raw);
        assert_eq!(store.attempts.load(Ordering::SeqCst), 0);
    }
}
