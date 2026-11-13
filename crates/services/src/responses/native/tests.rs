use super::*;

#[test]
fn selection_routes_every_canonical_openai_model_natively() {
    for model in [
        "openai/gpt-4.1",
        "openai/gpt-5.4",
        "openai/gpt-6.1-sol",
        "openai/gpt-6-astra",
        "openai/gpt-6-astra-2026-09-03",
        "openai/future-model",
    ] {
        assert!(selected(model), "{model}");
    }
    for model in [
        "custom/model",
        "anthropic/claude",
        "Qwen/model",
        "openai-compatible/model",
    ] {
        assert!(!selected(model), "{model}");
    }
}

#[test]
fn native_requests_default_to_explicit_no_store_and_preserve_provider_fields() {
    for mut body in [
        json!({}),
        json!({"store":null}),
        json!({"store":false}),
        json!({"store":false,"conversation":null,"previous_response_id":null,"background":null}),
    ] {
        body["reasoning"] = json!({"effort":"high"});
        body["include"] = json!(["reasoning.encrypted_content"]);
        let mut expected = body.clone();
        expected["store"] = json!(false);
        normalize_stateless_request(&mut body).unwrap();
        assert_eq!(body, expected);
        assert!(inference_providers::responses_raw::is_stateless(&body));
    }
}

#[test]
fn native_requests_reject_stateful_or_invalid_fields_before_dispatch() {
    for mut body in [
        json!(null),
        json!([]),
        json!({"store":true}),
        json!({"store":"false"}),
        json!({"store":false,"conversation":"conv_1"}),
        json!({"store":false,"previous_response_id":"resp_1"}),
        json!({"store":false,"background":true}),
    ] {
        assert!(normalize_stateless_request(&mut body).is_err(), "{body}");
    }
}

#[test]
fn native_max_tool_calls_matches_the_typed_hosted_tool_budget() {
    use crate::responses::models::{DEFAULT_MAX_HOSTED_TOOL_CALLS, MAX_HOSTED_TOOL_CALLS};
    let base = json!({"model":"openai/gpt-6-astra", "store":false, "input":"hi",
        "tools":[{"type":"web_search"}], "include":["web_search_call.action.sources"],
        "reasoning":{"effort":"high"}, "parallel_tool_calls":false});
    let forwarded = |max: Option<Value>| {
        let mut body = base.clone();
        if let Some(max) = max {
            body["max_tool_calls"] = max;
        }
        normalize_max_tool_calls(&mut body).map(|()| body)
    };
    for (requested, budget) in [
        (None, DEFAULT_MAX_HOSTED_TOOL_CALLS),
        (Some(Value::Null), DEFAULT_MAX_HOSTED_TOOL_CALLS),
        (Some(json!(1)), 1),
        (Some(json!(MAX_HOSTED_TOOL_CALLS)), MAX_HOSTED_TOOL_CALLS),
        (
            Some(json!(MAX_HOSTED_TOOL_CALLS + 1)),
            MAX_HOSTED_TOOL_CALLS,
        ),
        (Some(json!(i64::MAX)), MAX_HOSTED_TOOL_CALLS),
    ] {
        let body = forwarded(requested.clone()).unwrap();
        let mut expected = base.clone();
        expected["max_tool_calls"] = json!(budget);
        assert_eq!(body, expected, "{requested:?}: other fields preserved");
    }
    assert_eq!(DEFAULT_MAX_HOSTED_TOOL_CALLS, 10);
    assert_eq!(MAX_HOSTED_TOOL_CALLS, 20);
    for invalid in [
        json!(0),
        json!(-1),
        json!(i64::MIN),
        json!(u64::MAX),
        json!(1.5),
        json!(2.0),
        json!("5"),
        json!(true),
        json!([5]),
        json!({"max":5}),
    ] {
        assert_eq!(
            forwarded(Some(invalid.clone())),
            Err("max_tool_calls must be a positive integer"),
            "{invalid}"
        );
    }
}

#[test]
fn native_validation_accepts_replay_and_rejects_unsupported_service_tiers() {
    let mut body = json!({"model":"openai/gpt-6-astra", "store":false, "input":[
        {"type":"reasoning", "id":"rs_1", "encrypted_content":"opaque", "summary":[]},
        {"type":"function_call", "call_id":"call_1", "name":"lookup", "arguments":"{}"},
        {"type":"function_call_output", "call_id":"call_1", "output":"ok"}
    ], "tools":[{"type":"function", "name":"lookup", "strict":true}], "reasoning":{"effort":"high"}, "parallel_tool_calls":false});
    assert!(validate(&body).is_ok());
    body["service_tier"] = json!("priority");
    assert!(validate(&body).is_err());
}

#[test]
fn native_web_search_preserves_configuration_choice_and_include() {
    for choice in [json!("required"), json!({"type":"web_search"})] {
        let body = json!({
            "model":"openai/gpt-6-astra", "store":false, "input":"Find current sources.",
            "tools":[
                {"type":"web_search", "search_context_size":"high", "external_web_access":false,
                    "filters":{"allowed_domains":["example.com"]},
                    "user_location":{"type":"approximate", "country":"US", "city":"Boston"}},
                {"type":"function", "name":"lookup", "strict":true},
                {"type":"namespace", "name":"client", "tools":[
                    {"type":"custom", "name":"apply_patch", "format":{"type":"text"}}
                ]}
            ],
            "tool_choice":choice,
            "include":["web_search_call.action.sources", "reasoning.encrypted_content", "web_search_call.results"],
            "provider_extension":{"preserve":"exactly"}
        });
        let normalized =
            crate::responses::client_tools::normalize_native_request(body.clone()).unwrap();
        assert!(validate(&normalized).is_ok());
        assert_eq!(normalized, body);
    }
}

#[test]
fn native_web_search_replays_complete_output_without_transforming_it() {
    let body = json!({
        "model":"openai/gpt-6-astra", "store":false,
        "tools":[{"type":"web_search"}], "tool_choice":"none",
        "include":["web_search_call.action.sources", "reasoning.encrypted_content"],
        "input":[
            {"role":"user", "content":"Find a source."},
            {"type":"reasoning", "id":"rs_old", "summary":[], "encrypted_content":"opaque_fixture"},
            {"type":"web_search_call", "id":"ws_old", "status":"completed", "action":{
                "type":"search", "query":"Synthetic query", "queries":["Synthetic query"],
                "sources":[{"type":"url", "url":"https://example.com/source"}]
            }},
            {"type":"message", "id":"msg_old", "role":"assistant", "status":"completed", "phase":"final_answer",
                "content":[{"type":"output_text", "text":"A sourced answer.", "annotations":[{
                    "type":"url_citation", "url":"https://example.com/source", "title":"Synthetic source", "start_index":2, "end_index":9
                }]}]},
            {"role":"user", "content":"Summarize that answer without another search."}
        ]
    });
    let normalized =
        crate::responses::client_tools::normalize_native_request(body.clone()).unwrap();
    assert!(validate(&normalized).is_ok());
    assert_eq!(normalized, body);
}

#[test]
fn native_client_tools_preserve_grammar_namespaces_and_custom_replay() {
    let body = json!({
        "model": "openai/gpt-6-astra", "store": false,
        "input": [
            {"type":"reasoning", "id":"rs_1", "encrypted_content":"opaque", "summary":[]},
            {"type":"custom_tool_call", "id":"ct_1", "call_id":"call_patch", "name":"apply_patch", "input":"*** Begin Patch\n*** End Patch"},
            {"type":"custom_tool_call_output", "call_id":"call_patch", "output":"Done"}
        ],
        "tools": [
            {"type":"custom", "name":"apply_patch", "format":{
                "type":"grammar", "syntax":"lark", "definition":"start: /.+/s"
            }},
            {"type":"namespace", "name":"functions", "description":"Local commands", "tools":[
                {"type":"function", "name":"exec_command", "strict":true, "parameters":{
                    "type":"object", "properties":{"cmd":{"type":"string"}},
                    "required":["cmd"], "additionalProperties":false
                }}
            ]}
        ],
        "include":["reasoning.encrypted_content"],
        "reasoning":{"effort":"high"},
        "parallel_tool_calls":false,
        "provider_extension":{"preserve":"exactly"}
    });
    let normalized =
        crate::responses::client_tools::normalize_native_request(body.clone()).unwrap();
    assert!(validate(&normalized).is_ok());
    assert_eq!(
        normalized, body,
        "native tools and replay stay in their original wire shape"
    );
}

#[test]
fn fragmented_native_events_keep_usage_including_cache_writes() {
    let wire = concat!(
        "event: response.created\r\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\r\n\r\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"París\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"service_tier\":\"default\",\"usage\":{\"input_tokens\":100,\"output_tokens\":30,\"total_tokens\":130,\"input_tokens_details\":{\"cached_tokens\":40,\"cache_write_tokens\":20}}}}\n\n"
    );
    let mut parser = EventUsage::default();
    let mut released = Vec::new();
    for chunk in wire.as_bytes().chunks(3) {
        parser.push(chunk, &mut released).unwrap();
    }
    parser.finish_input(&mut released).unwrap();
    // Everything before the terminal event is released; the terminal event
    // (with its `event:` line) is withheld, and nothing is rewritten.
    assert!(!String::from_utf8_lossy(&released).contains("response.completed"));
    released.extend_from_slice(&parser.pending);
    assert_eq!(released, wire.as_bytes());
    assert!(parser.usage.terminal);
    assert_eq!(parser.usage.id.as_deref(), Some("resp_1"));
    let usage = parser.usage.tokens.unwrap();
    assert_eq!(usage.completion_tokens, 30);
    assert_eq!(usage.cached_tokens(), 40);
    assert_eq!(usage.cache_write_tokens(), 20);
}

#[test]
fn incomplete_unknown_reason_keeps_billable_usage() {
    let mut usage = Usage::default();
    usage
        .apply(
            &json!({"status":"incomplete", "incomplete_details":{"reason":"future_reason"},
        "usage":{"input_tokens":5,"output_tokens":8,"total_tokens":13}}),
        )
        .unwrap();
    assert!(usage.terminal);
    assert!(matches!(usage.reason, Some(StopReason::Incomplete)));
    assert_eq!(usage.tokens.unwrap().total_tokens, 13);
}

#[test]
fn native_stateless_requests_cannot_reference_upstream_stored_resources() {
    for input in [
        json!([{"type":"item_reference","id":"msg_shared"}]),
        json!([{"type":"file_search_call","id":"fs_shared","status":"completed"}]),
        json!([{"type":"mcp_call","id":"mcp_shared","status":"completed"}]),
        json!([{"type":"function_call_output","call_id":"call_1","output":[{"type":"input_image","file_id":"file_shared"}]}]),
        json!([{"type":"custom_tool_call_output","call_id":"call_patch","output":[{"type":"input_file","file_id":"file_shared"}]}]),
        json!([{"role":"user","content":[{"type":"input_image","file_id":"file_shared"}]}]),
        json!([{"role":"user","content":[{"type":"input_file","file_id":"file_shared"}]}]),
    ] {
        assert!(validate(&json!({"input":input})).is_err());
    }
    assert!(validate(&json!({"prompt":{"id":"pmpt_shared"}})).is_err());
}

#[test]
fn usage_observer_does_not_treat_truncation_as_completion() {
    let mut parser = EventUsage::default();
    let mut released = Vec::new();
    parser.push(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\n", &mut released).unwrap();
    parser.finish_input(&mut released).unwrap();
    assert!(!parser.usage.terminal);
    assert!(parser.usage.tokens.is_none());
    assert!(parser.push(b"data: {bad json}\n", &mut released).is_err());
    let mut usage = Usage::default();
    assert!(usage.apply(&json!({"status":"completed","usage":{"input_tokens":-1,"output_tokens":1,"total_tokens":0}})).is_err());
}

// ---------------------------------------------------------------------------
// Billing lifecycle of the native stream (synthetic upstream, scripted usage).
// ---------------------------------------------------------------------------

use crate::responses::models::{ResponseStatus, ResponseStreamEvent};
use crate::usage::{
    InferenceUsageHistoryQuery, InferenceUsageReportRow, OrganizationBalanceInfo,
    OrganizationCreditLimit, OrganizationLimit, RecordUsageApiRequest, UsageCheckResult,
    UsageDiscount, UsageError, UsageLogEntry,
};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex,
};

const CREATED: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_synthetic\",\"status\":\"in_progress\"}}\n\n";
const DELTA: &str = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"delta\":\"Hi\"}\n\n";
const COMPLETED_EVENT_LINE: &str = "event: response.completed\n";
const COMPLETED_DATA: &str = "data: {\"type\":\"response.completed\",\"sequence_number\":2,\"response\":{\"id\":\"resp_synthetic\",\"status\":\"completed\",\"service_tier\":\"default\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15}}}\n\n";
const SYNTHETIC_MODEL: &str = "openai/gpt-synthetic";

#[derive(Clone, Copy)]
enum WriteOutcome {
    Succeed,
    /// Fails before the repository write, as a missing model does.
    RejectBeforeWrite,
    /// Fails without committing, with an error that may follow a commit.
    Ambiguous(fn() -> UsageError),
    /// Commits the row, then reports an error.
    CommitThenFail,
    Panic,
    /// Commits after this delay.
    Slow(Duration),
    /// Never returns, as a hung query does.
    Hang,
}

/// Records every attempted write and, separately, every write that
/// actually committed.
struct ScriptedUsage {
    outcome: WriteOutcome,
    attempts: Mutex<Vec<RecordUsageServiceRequest>>,
    committed: Mutex<Vec<RecordUsageServiceRequest>>,
}

impl ScriptedUsage {
    fn new(outcome: WriteOutcome) -> Arc<Self> {
        Arc::new(Self {
            outcome,
            attempts: Mutex::new(Vec::new()),
            committed: Mutex::new(Vec::new()),
        })
    }
    fn attempts(&self) -> Vec<RecordUsageServiceRequest> {
        self.attempts.lock().unwrap().clone()
    }
    fn committed(&self) -> Vec<RecordUsageServiceRequest> {
        self.committed.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl UsageServiceTrait for ScriptedUsage {
    async fn record_usage(
        &self,
        request: RecordUsageServiceRequest,
    ) -> Result<UsageLogEntry, UsageError> {
        self.attempts.lock().unwrap().push(request.clone());
        match self.outcome {
            WriteOutcome::Succeed => {}
            WriteOutcome::RejectBeforeWrite => {
                return Err(UsageError::ModelNotFound("synthetic".into()))
            }
            WriteOutcome::Ambiguous(error) => return Err(error()),
            WriteOutcome::CommitThenFail => {
                self.committed.lock().unwrap().push(request);
                return Err(UsageError::InternalError("synthetic".into()));
            }
            WriteOutcome::Panic => panic!("synthetic usage task panic"),
            WriteOutcome::Slow(delay) => tokio::time::sleep(delay).await,
            WriteOutcome::Hang => std::future::pending::<()>().await,
        }
        self.committed.lock().unwrap().push(request.clone());
        crate::test_utils::MockUsageService
            .record_usage(request)
            .await
    }
    async fn record_usage_from_api(
        &self,
        _organization_id: Uuid,
        _workspace_id: Uuid,
        _api_key_id: Uuid,
        _request: RecordUsageApiRequest,
        _discount: Option<UsageDiscount>,
    ) -> Result<UsageLogEntry, UsageError> {
        unreachable!("native Responses records usage directly")
    }
    async fn check_can_use(&self, _organization_id: Uuid) -> Result<UsageCheckResult, UsageError> {
        unreachable!()
    }
    async fn get_balance(
        &self,
        _organization_id: Uuid,
    ) -> Result<Option<OrganizationBalanceInfo>, UsageError> {
        unreachable!()
    }
    async fn get_usage_history(
        &self,
        _organization_id: Uuid,
        _limit: Option<i64>,
        _offset: Option<i64>,
    ) -> Result<(Vec<UsageLogEntry>, i64), UsageError> {
        unreachable!()
    }
    async fn get_limit(
        &self,
        _organization_id: Uuid,
    ) -> Result<Option<OrganizationLimit>, UsageError> {
        unreachable!()
    }
    async fn get_credit_limits(
        &self,
        _organization_id: Uuid,
    ) -> Result<Vec<OrganizationCreditLimit>, UsageError> {
        unreachable!()
    }
    async fn get_usage_history_by_api_key(
        &self,
        _api_key_id: Uuid,
        _credit_type: Option<&str>,
        _limit: Option<i64>,
        _offset: Option<i64>,
    ) -> Result<(Vec<UsageLogEntry>, i64), UsageError> {
        unreachable!()
    }
    async fn get_api_key_usage_history_with_permissions(
        &self,
        _workspace_id: Uuid,
        _api_key_id: Uuid,
        _user_id: Uuid,
        _credit_type: Option<&str>,
        _limit: Option<i64>,
        _offset: Option<i64>,
    ) -> Result<(Vec<UsageLogEntry>, i64), UsageError> {
        unreachable!()
    }
    async fn get_costs_by_inference_ids(
        &self,
        _organization_id: Uuid,
        _inference_ids: Vec<Uuid>,
    ) -> Result<Vec<crate::usage::InferenceCost>, UsageError> {
        unreachable!()
    }
    async fn get_usage_by_model(
        &self,
        _organization_id: Uuid,
        _start_date: chrono::DateTime<chrono::Utc>,
        _end_date: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<crate::usage::UsageByModelEntry>, UsageError> {
        unreachable!()
    }
    async fn list_inference_usage_history(
        &self,
        _query: InferenceUsageHistoryQuery,
    ) -> Result<(Vec<InferenceUsageReportRow>, i64), UsageError> {
        unreachable!()
    }
}

fn billing(usage: &Arc<ScriptedUsage>, inference_type: InferenceType) -> Billing {
    Billing {
        usage_service: usage.clone(),
        attestation_service: Arc::new(crate::test_utils::MockAttestationService),
        inference_id: Uuid::new_v4(),
        request_hash: "synthetic-request-hash".into(),
        organization_id: Uuid::new_v4(),
        workspace_id: Uuid::new_v4(),
        api_key_id: Uuid::new_v4(),
        model_id: Uuid::new_v4(),
        model_name: SYNTHETIC_MODEL.into(),
        inference_type,
        attribution: ProviderAttribution::default(),
    }
}

fn body_of(chunks: Vec<&'static str>) -> ResponsesRawBody {
    Box::pin(futures_util::stream::iter(
        chunks
            .into_iter()
            .map(|chunk| Ok(Bytes::from_static(chunk.as_bytes()))),
    ))
}

/// Returns the stream, its billing id, and the concurrency slot counter.
fn usage_stream(
    usage: &Arc<ScriptedUsage>,
    body: ResponsesRawBody,
) -> (UsageStream, Uuid, Arc<AtomicU32>) {
    let billing = billing(usage, InferenceType::ChatCompletionStream);
    let inference_id = billing.inference_id;
    let slots = Arc::new(AtomicU32::new(1));
    let stream = UsageStream::new(
        body,
        billing,
        ConcurrentRequestGuard::new(slots.clone()),
        tokio::runtime::Handle::current(),
    );
    (stream, inference_id, slots)
}

/// All bytes the client receives, and how many error items ended the body.
async fn drain(stream: &mut UsageStream) -> (Vec<u8>, usize) {
    let mut bytes = Vec::new();
    let mut errors = 0;
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => bytes.extend_from_slice(&chunk),
            Err(_) => errors += 1,
        }
    }
    (bytes, errors)
}

/// Polls the stream past the released events up to the gated terminal event.
async fn advance_to_billing_wait(stream: &mut UsageStream) -> Vec<u8> {
    let mut released = Vec::new();
    released.extend_from_slice(&stream.next().await.unwrap().unwrap());
    released.extend_from_slice(&stream.next().await.unwrap().unwrap());
    assert!(
        futures::poll!(stream.next()).is_pending(),
        "the terminal event must wait for the usage write"
    );
    assert!(matches!(stream.state, StreamState::Billing { .. }));
    released
}

fn upstream_wire() -> String {
    [CREATED, DELTA, COMPLETED_EVENT_LINE, COMPLETED_DATA].concat()
}

#[tokio::test(start_paused = true)]
async fn streaming_success_releases_terminal_event_only_after_usage_is_recorded() {
    let usage = ScriptedUsage::new(WriteOutcome::Slow(Duration::from_secs(1)));
    let (mut stream, inference_id, slots) = usage_stream(
        &usage,
        body_of(vec![CREATED, DELTA, COMPLETED_EVENT_LINE, COMPLETED_DATA]),
    );

    let mut client = advance_to_billing_wait(&mut stream).await;
    assert_eq!(client, [CREATED, DELTA].concat().as_bytes());
    assert!(usage.committed().is_empty());
    assert_eq!(slots.load(Ordering::Acquire), 0, "slot released at EOF");

    let (rest, errors) = drain(&mut stream).await;
    assert_eq!(errors, 0);
    assert_eq!(
        rest,
        [COMPLETED_EVENT_LINE, COMPLETED_DATA].concat().as_bytes()
    );
    client.extend_from_slice(&rest);
    assert_eq!(
        client,
        upstream_wire().as_bytes(),
        "bytes pass through unchanged"
    );

    let committed = usage.committed();
    assert_eq!(committed.len(), 1);
    let record = &committed[0];
    assert_eq!(record.inference_id, Some(inference_id));
    assert_eq!(
        record.provider_request_id.as_deref(),
        Some("resp_synthetic")
    );
    assert_eq!((record.input_tokens, record.output_tokens), (10, 5));
    assert_eq!(record.stop_reason, Some(StopReason::Completed));
    assert_eq!(record.inference_type, InferenceType::ChatCompletionStream);
}

/// Parses the single `response.failed` frame that follows `prefix` as the
/// typed stream event, and checks the fields every billing failure carries.
fn parse_failure(client: &[u8], prefix: &str) -> (ResponseStreamEvent, String) {
    let text = String::from_utf8(client.to_vec()).unwrap();
    let failure = text
        .strip_prefix(prefix)
        .expect("events before the terminal event are delivered");
    assert!(!text.contains("response.completed"), "no success frame");
    let data = failure
        .strip_prefix("event: response.failed\ndata: ")
        .and_then(|rest| rest.strip_suffix("\n\n"))
        .expect("one complete response.failed frame");
    let event: ResponseStreamEvent =
        serde_json::from_str(data).expect("failure frame is a typed ResponseStreamEvent");
    assert_eq!(event.event_type, "response.failed");
    let response = event.response.as_ref().expect("response object");
    assert_eq!(response.object, "response");
    assert_eq!(response.status, ResponseStatus::Failed);
    assert_eq!(response.model, SYNTHETIC_MODEL);
    assert!(response.output.is_empty());
    assert!(response.tools.is_empty());
    assert!(!response.store);
    assert!(!response.background);
    assert!(response.instructions.is_none());
    assert!(response.user.is_none());
    assert!(response.incomplete_details.is_none());
    let error = response.error.as_ref().expect("fixed error object");
    assert_eq!(error.type_, "server_error");
    assert_eq!(error.code.as_deref(), Some("server_error"));
    assert_eq!(error.message, "Failed to record usage");
    assert!(error.param.is_none());
    assert_eq!(
        (
            response.usage.input_tokens,
            response.usage.output_tokens,
            response.usage.total_tokens
        ),
        (10, 5, 15)
    );
    (event, failure.to_string())
}

#[tokio::test]
async fn streaming_usage_failure_replaces_terminal_event_with_sanitized_failure() {
    let usage = ScriptedUsage::new(WriteOutcome::RejectBeforeWrite);
    let (mut stream, inference_id, _slots) = usage_stream(
        &usage,
        body_of(vec![CREATED, DELTA, COMPLETED_EVENT_LINE, COMPLETED_DATA]),
    );
    let (client, errors) = drain(&mut stream).await;
    assert_eq!(errors, 0, "the failure frame ends the body cleanly");
    let (event, _) = parse_failure(&client, &[CREATED, DELTA].concat());
    assert_eq!(
        event.sequence_number,
        Some(2),
        "replaces the terminal event"
    );
    let response = event.response.unwrap();
    assert_eq!(response.id, "resp_synthetic", "same id as response.created");
    let attempts = usage.attempts();
    assert_eq!(
        attempts.len(),
        1,
        "one write; retries live in the repository"
    );
    assert_eq!(attempts[0].inference_id, Some(inference_id));
    assert!(attempts[0].dedupe_on_conflict);
    assert!(usage.committed().is_empty());
}

#[tokio::test(start_paused = true)]
async fn streaming_slow_usage_write_commits_and_then_releases_success() {
    let delay = Duration::from_secs(30);
    let usage = ScriptedUsage::new(WriteOutcome::Slow(delay));
    let (mut stream, inference_id, _slots) = usage_stream(
        &usage,
        body_of(vec![CREATED, DELTA, COMPLETED_EVENT_LINE, COMPLETED_DATA]),
    );
    let started = tokio::time::Instant::now();
    let mut client = advance_to_billing_wait(&mut stream).await;
    // Past any client-side write limit, the terminal event is still held.
    tokio::time::sleep(Duration::from_secs(11)).await;
    assert!(futures::poll!(stream.next()).is_pending());
    assert!(usage.committed().is_empty());

    let (rest, errors) = drain(&mut stream).await;
    assert_eq!(errors, 0);
    assert!(started.elapsed() >= delay);
    client.extend_from_slice(&rest);
    assert_eq!(
        client,
        upstream_wire().as_bytes(),
        "success frame unchanged"
    );
    assert_eq!(usage.attempts().len(), 1);
    let committed = usage.committed();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].inference_id, Some(inference_id));
}

#[tokio::test]
async fn billing_failure_frame_discloses_no_response_content() {
    const CREATED_WITH_TIME: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_synthetic\",\"object\":\"response\",\"created_at\":1700000000,\"status\":\"in_progress\",\"model\":\"SENTINEL_MODEL\"}}\n\n";
    const COMPLETED_WITH_CONTENT: &str = concat!(
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":7,\"response\":{",
        "\"id\":\"resp_synthetic\",\"object\":\"response\",\"created_at\":1700000000,",
        "\"status\":\"completed\",\"model\":\"SENTINEL_MODEL\",",
        "\"instructions\":\"SENTINEL_INSTRUCTIONS\",\"user\":\"SENTINEL_USER\",",
        "\"metadata\":{\"note\":\"SENTINEL_METADATA\"},",
        "\"error\":{\"message\":\"SENTINEL_ERROR\"},",
        "\"tools\":[{\"type\":\"function\",\"name\":\"SENTINEL_TOOL\"}],",
        "\"tool_choice\":{\"type\":\"function\",\"name\":\"SENTINEL_TOOL\"},",
        "\"output\":[{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"completed\",",
        "\"content\":[{\"type\":\"output_text\",\"text\":\"SENTINEL_OUTPUT\",\"annotations\":[]}]}],",
        "\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15,",
        "\"input_tokens_details\":{\"cached_tokens\":4,\"SENTINEL_DETAIL\":1}}}}\n\n",
        // A trailing event after the terminal one must not move its sequence.
        "data: {\"type\":\"response.trailing\",\"sequence_number\":99}\n\n"
    );
    let usage = ScriptedUsage::new(WriteOutcome::RejectBeforeWrite);
    let (mut stream, _inference_id, _slots) = usage_stream(
        &usage,
        body_of(vec![CREATED_WITH_TIME, DELTA, COMPLETED_WITH_CONTENT]),
    );
    let (client, errors) = drain(&mut stream).await;
    assert_eq!(errors, 0);
    let (event, failure) = parse_failure(&client, &[CREATED_WITH_TIME, DELTA].concat());
    assert!(
        !failure.contains("SENTINEL"),
        "no upstream content: {failure}"
    );
    assert_eq!(event.sequence_number, Some(7));
    let response = event.response.unwrap();
    assert_eq!(response.id, "resp_synthetic");
    assert_eq!(response.created_at, 1_700_000_000);
    assert!(response.usage.input_tokens_details.is_none());
}

#[test]
fn billing_failure_id_falls_back_for_unusual_provider_ids() {
    let inference_id = Uuid::new_v4();
    let fallback = format!("resp_{}", inference_id.simple());
    assert_eq!(
        failure_response_id(Some("resp_abc-123"), inference_id),
        "resp_abc-123"
    );
    for observed in [
        None,
        Some(""),
        Some("resp_\"injected\""),
        Some("resp with space"),
    ] {
        assert_eq!(failure_response_id(observed, inference_id), fallback);
    }
    let long = "r".repeat(MAX_RESPONSE_ID_LEN + 1);
    assert_eq!(failure_response_id(Some(&long), inference_id), fallback);
}

#[tokio::test(start_paused = true)]
async fn client_disconnect_while_awaiting_usage_records_exactly_once() {
    let usage = ScriptedUsage::new(WriteOutcome::Slow(Duration::from_secs(30)));
    let (mut stream, inference_id, _slots) = usage_stream(
        &usage,
        body_of(vec![CREATED, DELTA, COMPLETED_EVENT_LINE, COMPLETED_DATA]),
    );
    advance_to_billing_wait(&mut stream).await;
    drop(stream);
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(usage.attempts().len(), 1);
    let committed = usage.committed();
    assert_eq!(
        committed.len(),
        1,
        "the in-flight write survives the disconnect"
    );
    assert_eq!(committed[0].inference_id, Some(inference_id));
    assert_eq!(committed[0].stop_reason, Some(StopReason::Completed));
}

#[tokio::test]
async fn client_disconnect_before_upstream_eof_keeps_observed_usage() {
    let usage = ScriptedUsage::new(WriteOutcome::Succeed);
    // The terminal event arrived but upstream never closes.
    let body: ResponsesRawBody = Box::pin(
        futures_util::stream::iter(
            [CREATED, COMPLETED_EVENT_LINE, COMPLETED_DATA]
                .map(|chunk| Ok(Bytes::from_static(chunk.as_bytes()))),
        )
        .chain(futures_util::stream::pending()),
    );
    let (mut stream, inference_id, slots) = usage_stream(&usage, body);
    assert_eq!(stream.next().await.unwrap().unwrap(), CREATED.as_bytes());
    assert!(futures::poll!(stream.next()).is_pending());
    drop(stream);
    assert_eq!(slots.load(Ordering::Acquire), 0);
    tokio::time::timeout(Duration::from_secs(5), async {
        while usage.committed().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("observed usage recorded after disconnect");
    let committed = usage.committed();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].inference_id, Some(inference_id));
    assert_eq!(
        (committed[0].input_tokens, committed[0].output_tokens),
        (10, 5)
    );
    assert_eq!(committed[0].stop_reason, Some(StopReason::ClientDisconnect));
}

#[tokio::test]
async fn provider_error_without_usage_keeps_its_own_frame() {
    const ERROR: &str = "event: error\ndata: {\"type\":\"error\",\"sequence_number\":1,\"code\":\"server_error\",\"message\":\"synthetic\"}\n\n";
    // A failing usage service proves no write gates this frame.
    let usage = ScriptedUsage::new(WriteOutcome::RejectBeforeWrite);
    let (mut stream, _inference_id, _slots) = usage_stream(&usage, body_of(vec![CREATED, ERROR]));
    let (client, errors) = drain(&mut stream).await;
    assert_eq!(errors, 0);
    assert_eq!(client, [CREATED, ERROR].concat().as_bytes());
    assert!(usage.attempts().is_empty());
}

#[tokio::test]
async fn terminal_event_without_usage_is_never_released() {
    const COMPLETED_NO_USAGE: &str = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_synthetic\",\"status\":\"completed\"}}\n\n";
    let usage = ScriptedUsage::new(WriteOutcome::Succeed);
    let (mut stream, _inference_id, _slots) =
        usage_stream(&usage, body_of(vec![CREATED, COMPLETED_NO_USAGE]));
    let (client, errors) = drain(&mut stream).await;
    assert_eq!(errors, 1);
    assert_eq!(client, CREATED.as_bytes());
}

fn json_upstream(body: Value) -> ResponsesRawResponse {
    ResponsesRawResponse {
        status: reqwest::StatusCode::OK,
        headers: reqwest::header::HeaderMap::new(),
        body: Box::pin(futures_util::stream::once(async move {
            Ok(Bytes::from(body.to_string()))
        })),
    }
}

fn completed_json() -> Value {
    json!({"id":"resp_synthetic","object":"response","status":"completed",
        "usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}})
}

#[tokio::test]
async fn non_streaming_records_usage_under_the_returned_billing_id() {
    let usage = ScriptedUsage::new(WriteOutcome::Succeed);
    let billing = billing(&usage, InferenceType::ChatCompletion);
    let expected = billing.inference_id;
    let slots = Arc::new(AtomicU32::new(1));
    let guard = ConcurrentRequestGuard::new(slots.clone());
    let response = account(json_upstream(completed_json()), false, billing, guard)
        .await
        .unwrap();
    assert_eq!(response.inference_id, expected);
    let committed = usage.committed();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].inference_id, Some(expected));
    assert_eq!(
        committed[0].provider_request_id.as_deref(),
        Some("resp_synthetic")
    );
    assert_eq!(slots.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn non_streaming_usage_failure_returns_no_response_body() {
    let usage = ScriptedUsage::new(WriteOutcome::RejectBeforeWrite);
    let billing = billing(&usage, InferenceType::ChatCompletion);
    let inference_id = billing.inference_id;
    let guard = ConcurrentRequestGuard::new(Arc::new(AtomicU32::new(1)));
    let result = account(json_upstream(completed_json()), false, billing, guard).await;
    assert!(matches!(
        result,
        Err(NativeResponsesError::BillingOutcome {
            inference_id: id,
            message: "Failed to record Responses usage",
        }) if id == inference_id
    ));
    assert!(usage.committed().is_empty());
}

#[tokio::test]
async fn non_streaming_commit_then_error_reports_unknown_outcome_not_failure() {
    let usage = ScriptedUsage::new(WriteOutcome::CommitThenFail);
    let billing = billing(&usage, InferenceType::ChatCompletion);
    let inference_id = billing.inference_id;
    let guard = ConcurrentRequestGuard::new(Arc::new(AtomicU32::new(1)));
    let result = account(json_upstream(completed_json()), false, billing, guard).await;
    assert!(matches!(
        result,
        Err(NativeResponsesError::BillingOutcome {
            inference_id: id,
            message: "Responses usage outcome is unavailable",
        }) if id == inference_id
    ));
    assert_eq!(usage.attempts().len(), 1);
    let committed = usage.committed();
    assert_eq!(committed.len(), 1, "the row exists despite the error");
    assert_eq!(committed[0].inference_id, Some(inference_id));
}

#[test]
fn only_prewrite_usage_errors_are_definite_rejections() {
    let s = || "synthetic".to_string();
    for error in [
        UsageError::ModelNotFound(s()),
        UsageError::CostCalculationOverflow(s()),
        UsageError::ValidationError(s()),
    ] {
        assert_eq!(BillingVerdict::of_error(&error), BillingVerdict::Rejected);
    }
    for error in [
        UsageError::InternalError(s()),
        UsageError::ReportingTimeout,
        UsageError::LimitExceeded(s()),
        UsageError::Unauthorized(s()),
        UsageError::NotFound(s()),
    ] {
        assert_eq!(BillingVerdict::of_error(&error), BillingVerdict::Unknown);
    }
}

/// Drains a stream whose usage write ended ambiguously and checks the client
/// saw only the pre-terminal events and one transport error.
async fn assert_ambiguous_stream_withholds_terminal(outcome: WriteOutcome) -> Arc<ScriptedUsage> {
    let usage = ScriptedUsage::new(outcome);
    let (mut stream, inference_id, _slots) = usage_stream(
        &usage,
        body_of(vec![CREATED, DELTA, COMPLETED_EVENT_LINE, COMPLETED_DATA]),
    );
    let (client, errors) = drain(&mut stream).await;
    assert_eq!(errors, 1, "an ambiguous outcome ends the body as an error");
    assert_eq!(
        client,
        [CREATED, DELTA].concat().as_bytes(),
        "neither response.completed nor response.failed is sent"
    );
    let attempts = usage.attempts();
    assert_eq!(attempts.len(), 1, "a single service call");
    assert_eq!(attempts[0].inference_id, Some(inference_id));
    usage
}

#[tokio::test]
async fn streaming_commit_then_error_withholds_terminal_without_claiming_failure() {
    let usage = assert_ambiguous_stream_withholds_terminal(WriteOutcome::CommitThenFail).await;
    assert_eq!(usage.committed().len(), 1, "the row was committed");
}

#[tokio::test]
async fn streaming_ambiguous_errors_withhold_terminal_without_claiming_failure() {
    for error in [
        (|| UsageError::InternalError("synthetic".into())) as fn() -> UsageError,
        || UsageError::ReportingTimeout,
    ] {
        assert_ambiguous_stream_withholds_terminal(WriteOutcome::Ambiguous(error)).await;
    }
}

#[tokio::test]
async fn billing_task_panic_ends_stream_without_a_failure_or_success_frame() {
    let usage = ScriptedUsage::new(WriteOutcome::Panic);
    let (mut stream, _inference_id, _slots) = usage_stream(
        &usage,
        body_of(vec![CREATED, DELTA, COMPLETED_EVENT_LINE, COMPLETED_DATA]),
    );
    let (client, errors) = drain(&mut stream).await;
    assert_eq!(
        errors, 1,
        "the unknown outcome must end the stream as an error"
    );
    assert_eq!(client, [CREATED, DELTA].concat().as_bytes());
    assert_eq!(usage.attempts().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn non_streaming_verdict_wait_is_bounded_and_reports_unknown() {
    let usage = ScriptedUsage::new(WriteOutcome::Hang);
    let billing = billing(&usage, InferenceType::ChatCompletion);
    let inference_id = billing.inference_id;
    let guard = ConcurrentRequestGuard::new(Arc::new(AtomicU32::new(1)));
    let started = tokio::time::Instant::now();
    let result = account(json_upstream(completed_json()), false, billing, guard).await;
    assert_eq!(started.elapsed(), BILLING_VERDICT_WAIT);
    assert!(matches!(
        result,
        Err(NativeResponsesError::BillingOutcome {
            inference_id: id,
            message: "Responses usage outcome is unavailable",
        }) if id == inference_id
    ));
    assert_eq!(usage.attempts().len(), 1);
    assert!(usage.committed().is_empty());
}

#[tokio::test(start_paused = true)]
async fn non_streaming_verdict_timeout_does_not_cancel_the_write() {
    let delay = BILLING_VERDICT_WAIT + Duration::from_secs(10);
    let usage = ScriptedUsage::new(WriteOutcome::Slow(delay));
    let billing = billing(&usage, InferenceType::ChatCompletion);
    let inference_id = billing.inference_id;
    let guard = ConcurrentRequestGuard::new(Arc::new(AtomicU32::new(1)));
    let result = account(json_upstream(completed_json()), false, billing, guard).await;
    assert!(matches!(
        result,
        Err(NativeResponsesError::BillingOutcome {
            message: "Responses usage outcome is unavailable",
            ..
        })
    ));
    assert!(usage.committed().is_empty(), "the waiter gave up first");
    tokio::time::sleep(delay).await;
    let committed = usage.committed();
    assert_eq!(committed.len(), 1, "the detached write still commits");
    assert_eq!(committed[0].inference_id, Some(inference_id));
}

#[tokio::test(start_paused = true)]
async fn streaming_verdict_wait_is_bounded_and_withholds_terminal() {
    let usage = ScriptedUsage::new(WriteOutcome::Hang);
    let (mut stream, _inference_id, _slots) = usage_stream(
        &usage,
        body_of(vec![CREATED, DELTA, COMPLETED_EVENT_LINE, COMPLETED_DATA]),
    );
    let mut client = advance_to_billing_wait(&mut stream).await;
    let started = tokio::time::Instant::now();
    tokio::time::sleep(BILLING_VERDICT_WAIT - Duration::from_secs(1)).await;
    assert!(
        futures::poll!(stream.next()).is_pending(),
        "still waiting before the bound"
    );
    let (rest, errors) = drain(&mut stream).await;
    assert_eq!(started.elapsed(), BILLING_VERDICT_WAIT);
    assert_eq!(errors, 1, "the unknown outcome ends the body as an error");
    client.extend_from_slice(&rest);
    assert_eq!(
        client,
        [CREATED, DELTA].concat().as_bytes(),
        "neither response.completed nor response.failed is sent"
    );
    assert_eq!(usage.attempts().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn streaming_verdict_timeout_does_not_cancel_the_write() {
    let delay = BILLING_VERDICT_WAIT + Duration::from_secs(10);
    let usage = ScriptedUsage::new(WriteOutcome::Slow(delay));
    let (mut stream, inference_id, _slots) = usage_stream(
        &usage,
        body_of(vec![CREATED, DELTA, COMPLETED_EVENT_LINE, COMPLETED_DATA]),
    );
    let (client, errors) = drain(&mut stream).await;
    assert_eq!(errors, 1);
    assert_eq!(client, [CREATED, DELTA].concat().as_bytes());
    assert!(usage.committed().is_empty(), "the waiter gave up first");
    drop(stream);
    tokio::time::sleep(delay).await;
    let committed = usage.committed();
    assert_eq!(committed.len(), 1, "the detached write still commits");
    assert_eq!(committed[0].inference_id, Some(inference_id));
    assert_eq!(committed[0].stop_reason, Some(StopReason::Completed));
}
