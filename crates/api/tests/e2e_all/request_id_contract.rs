use crate::common::{
    get_api_key_for_org, get_session_id, list_workspaces, seed_models, setup_org_with_credits,
    setup_qwen_model, setup_test_server_with_pool, setup_test_server_with_search_providers,
    MockWebSearchProvider, E2E_PRIVACY_FILTER_MODEL_NAME, E2E_QWEN_IMAGE_MODEL_NAME,
    E2E_QWEN_MODEL_NAME, E2E_QWEN_RERANKER_MODEL_NAME, MOCK_USER_AGENT,
};
use api::models::BatchUpdateModelApiRequest;
use axum::http::Method;
use inference_providers::mock::{MockProvider, TracingCall};
use inference_providers::tracing_context::TracingContext;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use uuid::Uuid;

const REQUEST_ID_HEADER: &str = "x-request-id";
const ORG_ID_HEADER: &str = "x-org-id";
const WORKSPACE_ID_HEADER: &str = "x-workspace-id";
static DEV_ENV: OnceLock<()> = OnceLock::new();

fn allow_debug_attestation_keys() {
    DEV_ENV.get_or_init(|| {
        std::env::set_var("DEV", "1");
        std::env::set_var("BRAVE_SEARCH_PRO_API_KEY", "request-id-contract-test");
    });
}

async fn setup_request_id_server() -> (
    axum_test::TestServer,
    Arc<inference_providers::mock::MockProvider>,
) {
    allow_debug_attestation_keys();
    let (server, _, mock_provider) = setup_test_server_with_search_providers(
        Arc::new(MockWebSearchProvider::default_results()),
        None,
    )
    .await;
    (server, mock_provider)
}

fn request_id(response: &axum_test::TestResponse) -> Uuid {
    let value = response
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|header| header.to_str().ok())
        .expect("response should include x-request-id");
    Uuid::parse_str(value).expect("x-request-id should be a UUID")
}

fn assert_uuid_response_id(label: &str, response: &axum_test::TestResponse) -> Uuid {
    let id = request_id(response);
    println!(
        "request_id_contract {label}: status={} x-request-id={id}",
        response.status_code()
    );
    id
}

fn header_value(response: &axum_test::TestResponse, name: &str) -> String {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .expect("response should include expected header")
        .to_string()
}

fn assert_header_csv_contains(response: &axum_test::TestResponse, name: &str, expected: &str) {
    let header = header_value(response, name);
    assert!(
        header
            .split(',')
            .map(str::trim)
            .any(|part| part == "*" || part.eq_ignore_ascii_case(expected)),
        "{name} should include {expected}; actual value: {header}"
    );
}

#[tokio::test]
async fn test_request_id_contract_for_public_and_error_surfaces() {
    // Given
    let (server, _) = setup_request_id_server().await;
    let valid_id = Uuid::new_v4();
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let model = setup_qwen_model(&server).await;

    // When
    let health = server
        .get("/v1/health")
        .add_header(REQUEST_ID_HEADER, valid_id.to_string())
        .await;
    let invalid = server
        .get("/v1/health")
        .add_header(REQUEST_ID_HEADER, "not-a-uuid")
        .await;
    let absent = server.get("/v1/health").await;
    let unknown = server.get("/v1/unknown-route-for-request-id").await;
    let auth = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "request-id-contract",
            "messages": [{"role": "user", "content": "auth"}]
        }))
        .await;
    let validation = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": []
        }))
        .await;

    // Then
    assert_eq!(
        assert_uuid_response_id("success valid reuse", &health),
        valid_id
    );

    let invalid_id = assert_uuid_response_id("invalid replacement", &invalid);
    assert_ne!(invalid_id.to_string(), "not-a-uuid");

    let absent_id = assert_uuid_response_id("generated absent", &absent);
    assert_ne!(absent_id, invalid_id);

    assert_uuid_response_id("unknown route error", &unknown);
    assert_uuid_response_id("auth error", &auth);
    assert_eq!(validation.status_code(), 400);
    assert_uuid_response_id("validation error", &validation);
}

#[tokio::test]
async fn test_request_id_contract_for_cors_and_streaming_surfaces() {
    // Given
    let (server, mock_provider) = setup_request_id_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace_id = workspaces
        .first()
        .expect("test org should have a default workspace")
        .id
        .clone();
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    let model = setup_qwen_model(&server).await;
    let client_org_id = Uuid::new_v4().to_string();
    let client_workspace_id = Uuid::new_v4().to_string();
    let inbound_request_id = Uuid::new_v4();

    // When
    let cors = server
        .method(Method::OPTIONS, "/v1/health")
        .add_header("Origin", "http://localhost:3000")
        .add_header("Access-Control-Request-Method", "GET")
        .add_header("Access-Control-Request-Headers", REQUEST_ID_HEADER)
        .await;
    let cors_actual = server
        .get("/v1/health")
        .add_header("Origin", "http://localhost:3000")
        .await;
    let browser_origin = "https://www.typingmind.com";
    let browser_preflight = server
        .method(Method::OPTIONS, "/v1/chat/completions")
        .add_header("Origin", browser_origin)
        .add_header("Access-Control-Request-Method", "POST")
        .add_header(
            "Access-Control-Request-Headers",
            "authorization,content-type,x-request-id",
        )
        .await;
    let browser_models = server
        .get("/v1/models")
        .add_header("Origin", browser_origin)
        .await;
    let browser_auth_error = server
        .post("/v1/chat/completions")
        .add_header("Origin", browser_origin)
        .add_header("Authorization", "Bearer invalidkey")
        .json(&serde_json::json!({"model": "test", "messages": []}))
        .await;
    let streaming = server
        .post("/v1/chat/completions")
        .add_header("Origin", browser_origin)
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header(REQUEST_ID_HEADER, inbound_request_id.to_string())
        .add_header(ORG_ID_HEADER, client_org_id.clone())
        .add_header(WORKSPACE_ID_HEADER, client_workspace_id.clone())
        .json(&serde_json::json!({
            "model": model,
            "stream": true,
            "rid": "client-rid",
            "request_id": "client-request-id",
            "x_request_id": inbound_request_id.to_string(),
            "x_org_id": client_org_id.clone(),
            "x_workspace_id": client_workspace_id.clone(),
            "messages": [{"role": "user", "content": "stream"}]
        }))
        .await;

    // Then
    assert_uuid_response_id("CORS preflight", &cors);
    assert_header_csv_contains(&cors, "access-control-allow-headers", REQUEST_ID_HEADER);
    println!(
        "request_id_contract CORS allow status={} x-request-id_allowed=true access-control-allow-headers={}",
        cors.status_code(),
        header_value(&cors, "access-control-allow-headers")
    );
    assert_uuid_response_id("CORS actual", &cors_actual);
    assert_header_csv_contains(
        &cors_actual,
        "access-control-expose-headers",
        REQUEST_ID_HEADER,
    );
    println!(
        "request_id_contract CORS expose status={} x-request-id_exposed=true access-control-expose-headers={}",
        cors_actual.status_code(),
        header_value(&cors_actual, "access-control-expose-headers")
    );
    assert_eq!(browser_preflight.status_code(), 200);
    assert_eq!(
        header_value(&browser_preflight, "access-control-allow-headers"),
        "authorization,content-type,x-request-id",
        "Authorization must be explicitly allowed, not covered by a wildcard",
    );
    assert_eq!(browser_models.status_code(), 200);
    assert_eq!(browser_auth_error.status_code(), 401);
    for response in [
        &browser_preflight,
        &browser_models,
        &browser_auth_error,
        &streaming,
    ] {
        assert_eq!(
            header_value(response, "access-control-allow-origin"),
            browser_origin
        );
        assert!(!response
            .headers()
            .contains_key("access-control-allow-credentials"));
    }
    let selected_id = assert_uuid_response_id("streaming success", &streaming);
    assert_eq!(streaming.status_code(), 200);
    assert_eq!(selected_id, inbound_request_id);
    println!(
        "request_id_contract streaming headers observed before body chunks status={} x-request-id={selected_id}",
        streaming.status_code()
    );
    let streaming_body = streaming.text();
    assert!(
        streaming_body.contains("[DONE]"),
        "streaming test must drain the finite SSE response"
    );
    let expected = TenantIds {
        org_id: parse_uuid(&org.id),
        workspace_id: parse_uuid(&workspace_id),
    };
    let context = assert_calls_carry_tenant(
        "POST /v1/chat/completions (stream)",
        &mock_provider.tracing_calls(),
        &["chat_completion_stream"],
        expected,
        inbound_request_id,
    );
    assert_ne!(
        context.org_id.to_string(),
        client_org_id,
        "client tenant headers must not propagate"
    );
    assert_ne!(
        context.workspace_id.to_string(),
        client_workspace_id,
        "client workspace headers must not propagate"
    );
    let params = mock_provider
        .last_chat_params()
        .await
        .expect("streaming request should reach mock provider");
    assert_no_client_ids_in_extra("POST /v1/chat/completions (stream)", &params.extra, context);
    println!("request_id_contract client tenant headers ignored");
}

#[tokio::test]
async fn test_request_id_contract_for_non_streaming_chat() {
    // Given
    let (server, mock_provider) = setup_request_id_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    let workspace_id = list_workspaces(&server, org.id.clone())
        .await
        .first()
        .expect("test org should have a default workspace")
        .id
        .clone();
    let model = setup_qwen_model(&server).await;
    let inbound_request_id = Uuid::new_v4();
    let client_org_id = Uuid::new_v4().to_string();

    // When
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header(REQUEST_ID_HEADER, inbound_request_id.to_string())
        .add_header(ORG_ID_HEADER, client_org_id.clone())
        .json(&serde_json::json!({
            "model": model,
            "stream": false,
            "rid": "client-rid",
            "request_id": "client-request-id",
            "x_request_id": inbound_request_id.to_string(),
            "x_org_id": client_org_id,
            "x_workspace_id": Uuid::new_v4().to_string(),
            "messages": [{"role": "user", "content": "non-stream"}]
        }))
        .await;

    // Then
    assert_eq!(response.status_code(), 200);
    assert_eq!(
        assert_uuid_response_id("non-streaming success", &response),
        inbound_request_id
    );
    let calls = wait_for_operations(&mock_provider, &["chat_completion", "get_signature"]).await;
    let context = assert_calls_carry_tenant(
        "POST /v1/chat/completions",
        &calls,
        &["chat_completion", "get_signature"],
        TenantIds {
            org_id: parse_uuid(&org.id),
            workspace_id: parse_uuid(&workspace_id),
        },
        inbound_request_id,
    );
    let params = mock_provider
        .last_chat_params()
        .await
        .expect("non-streaming request should reach mock provider");
    assert_no_client_ids_in_extra("POST /v1/chat/completions", &params.extra, context);
}

/// Engine request ids a client could set in a body. The route drops them
/// before the provider is called.
const ENGINE_ID_KEYS: [&str; 2] = ["rid", "request_id"];

/// Body keys that name the tracing ids. Cloud-api never writes them into the
/// provider's `extra` map; the ids travel in the tracing context instead. A
/// client body field of the same name still reaches `extra`; the provider
/// drops it when building its upstream request (covered by the provider unit
/// tests), so at this boundary only its value can be checked.
const TRACING_ID_KEYS: [&str; 3] = ["x_request_id", "x_org_id", "x_workspace_id"];

/// How long a test waits for deferred inference work (signature fetches) to
/// reach the mock provider.
const DEFERRED_CALL_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TenantIds {
    org_id: Uuid,
    workspace_id: Uuid,
}

fn parse_uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).expect("test fixture id should be a UUID")
}

/// Neither the engine ids nor any of the request's real tracing ids reach the
/// provider's `extra` map. A tracing key there can only hold the client's own
/// value.
fn assert_no_client_ids_in_extra(
    route: &str,
    extra: &std::collections::HashMap<String, serde_json::Value>,
    context: TracingContext,
) {
    for key in ENGINE_ID_KEYS {
        assert!(
            !extra.contains_key(key),
            "{route}: client engine id `{key}` must not reach the provider"
        );
    }
    let real_ids =
        [context.request_id, context.org_id, context.workspace_id].map(|id| id.to_string());
    for key in TRACING_ID_KEYS {
        if let Some(value) = extra.get(key).and_then(serde_json::Value::as_str) {
            assert!(
                !real_ids.iter().any(|id| id == value),
                "{route}: cloud-api must not put tracing ids into the provider's extra (`{key}`)"
            );
        }
    }
}

/// Wait until every operation in `expected` has been recorded by `mock`, then
/// return all recorded calls. Some calls (signature fetches) run after the
/// response is returned, so the wait keeps a missing one from passing unseen.
async fn wait_for_operations(mock: &MockProvider, expected: &[&str]) -> Vec<TracingCall> {
    let deadline = tokio::time::Instant::now() + DEFERRED_CALL_DEADLINE;
    loop {
        let calls = mock.tracing_calls();
        if expected
            .iter()
            .all(|operation| calls.iter().any(|call| call.operation == *operation))
            || tokio::time::Instant::now() >= deadline
        {
            return calls;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Every call carries the authenticated tenant and one server-chosen request
/// id, and every expected operation was made. Returns that context.
fn assert_calls_carry_tenant(
    route: &str,
    calls: &[TracingCall],
    expected_operations: &[&str],
    expected: TenantIds,
    inbound_request_id: Uuid,
) -> TracingContext {
    check_calls_carry_tenant(
        route,
        calls,
        expected_operations,
        expected,
        inbound_request_id,
    )
    .unwrap_or_else(|failure| panic!("{failure}"))
}

/// [`assert_calls_carry_tenant`] as a result, so a table can report every
/// failing row at once.
fn check_calls_carry_tenant(
    route: &str,
    calls: &[TracingCall],
    expected_operations: &[&str],
    expected: TenantIds,
    inbound_request_id: Uuid,
) -> Result<TracingContext, String> {
    let operations: Vec<&str> = calls.iter().map(|call| call.operation).collect();
    for operation in expected_operations {
        if !operations.contains(operation) {
            return Err(format!(
                "{route}: expected a `{operation}` inference call; recorded {operations:?}"
            ));
        }
    }
    let first = calls
        .first()
        .ok_or_else(|| format!("{route}: no inference call was recorded"))?;
    let mut request_id = None;
    for call in calls {
        let context = call.context.ok_or_else(|| {
            format!(
                "{route}: `{}` ran without tenant ids; recorded {operations:?}",
                call.operation
            )
        })?;
        if (context.org_id, context.workspace_id) != (expected.org_id, expected.workspace_id) {
            return Err(format!(
                "{route}: `{}` must carry the authenticated organization and workspace",
                call.operation
            ));
        }
        if *request_id.get_or_insert(context.request_id) != context.request_id {
            return Err(format!(
                "{route}: `{}` must carry the same request id as the request's other calls",
                call.operation
            ));
        }
    }
    let context = first
        .context
        .ok_or_else(|| format!("{route}: `{}` ran without tenant ids", first.operation))?;
    if context.request_id == inbound_request_id {
        return Err(format!(
            "{route}: the provider must receive the server's request id, not the inbound one"
        ));
    }
    Ok(context)
}

#[tokio::test]
async fn test_request_id_contract_provider_receives_the_logged_request_id() {
    // Given
    let (server, mock_provider) = setup_request_id_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    let model = setup_qwen_model(&server).await;

    // When: no inbound id, so the response reports the server's own id.
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "stream": true,
            "messages": [{"role": "user", "content": "server-id"}]
        }))
        .await;

    // Then
    assert_eq!(response.status_code(), 200);
    assert!(response.text().contains("[DONE]"));
    let server_request_id = request_id(&response);
    let params = mock_provider
        .last_chat_params()
        .await
        .expect("request should reach mock provider");
    for key in TRACING_ID_KEYS {
        assert!(
            !params.extra.contains_key(key),
            "cloud-api must not put `{key}` into the provider's extra"
        );
    }
    let calls = mock_provider.tracing_calls();
    assert!(!calls.is_empty(), "the request should reach the provider");
    for call in calls {
        assert_eq!(
            call.context.map(|context| context.request_id),
            Some(server_request_id),
            "`{}` must carry the request id the server reports",
            call.operation
        );
    }
}

/// Every inference route that accepts an API key and can reach a self-hosted
/// provider. `POST /v1/messages` is not listed: it is off by default and only
/// serves external Anthropic-backed models, which never read the tracing
/// context. `POST /v1/systemone` is left out on purpose: whether its backend
/// receives tenant ids is undecided, and the NEAR provider currently sends
/// none on that path (covered by the provider unit test
/// `systemone_in_scope_sends_no_tenant_ids`).
#[derive(Debug, Clone, Copy)]
enum InferenceRoute {
    ChatCompletion,
    ChatCompletionStream,
    ChatCompletionAutoRedact,
    TextCompletion,
    Responses,
    PrivacyClassify,
    PrivacyRedact,
    Embeddings,
    Rerank,
    Score,
    AudioTranscription,
    ImageGeneration,
    ImageEdit,
}

const INFERENCE_ROUTES: [InferenceRoute; 13] = [
    InferenceRoute::ChatCompletion,
    InferenceRoute::ChatCompletionStream,
    InferenceRoute::ChatCompletionAutoRedact,
    InferenceRoute::TextCompletion,
    InferenceRoute::Responses,
    InferenceRoute::PrivacyClassify,
    InferenceRoute::PrivacyRedact,
    InferenceRoute::Embeddings,
    InferenceRoute::Rerank,
    InferenceRoute::Score,
    InferenceRoute::AudioTranscription,
    InferenceRoute::ImageGeneration,
    InferenceRoute::ImageEdit,
];

/// What a row sends besides its own body: the client-chosen ids that must not
/// reach the provider.
struct ClientIds {
    request_id: Uuid,
    org_id: Uuid,
    workspace_id: Uuid,
}

impl InferenceRoute {
    fn label(self) -> &'static str {
        match self {
            Self::ChatCompletion => "POST /v1/chat/completions",
            Self::ChatCompletionStream => "POST /v1/chat/completions (stream)",
            Self::ChatCompletionAutoRedact => "POST /v1/chat/completions (x-auto-redact)",
            Self::TextCompletion => "POST /v1/completions",
            Self::Responses => "POST /v1/responses",
            Self::PrivacyClassify => "POST /v1/privacy/classify",
            Self::PrivacyRedact => "POST /v1/privacy/redact",
            Self::Embeddings => "POST /v1/embeddings",
            Self::Rerank => "POST /v1/rerank",
            Self::Score => "POST /v1/score",
            Self::AudioTranscription => "POST /v1/audio/transcriptions",
            Self::ImageGeneration => "POST /v1/images/generations",
            Self::ImageEdit => "POST /v1/images/edits",
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::ChatCompletion | Self::ChatCompletionStream | Self::ChatCompletionAutoRedact => {
                "/v1/chat/completions"
            }
            Self::TextCompletion => "/v1/completions",
            Self::Responses => "/v1/responses",
            Self::PrivacyClassify => "/v1/privacy/classify",
            Self::PrivacyRedact => "/v1/privacy/redact",
            Self::Embeddings => "/v1/embeddings",
            Self::Rerank => "/v1/rerank",
            Self::Score => "/v1/score",
            Self::AudioTranscription => "/v1/audio/transcriptions",
            Self::ImageGeneration => "/v1/images/generations",
            Self::ImageEdit => "/v1/images/edits",
        }
    }

    /// Calls the request must make, including deferred ones.
    fn expected_operations(self) -> &'static [&'static str] {
        match self {
            Self::ChatCompletion => &["chat_completion", "get_signature"],
            Self::ChatCompletionStream => &["chat_completion_stream", "get_signature"],
            Self::ChatCompletionAutoRedact => &["privacy_classify_raw", "chat_completion"],
            Self::TextCompletion => &["chat_completion"],
            Self::Responses => &["chat_completion_stream", "get_signature"],
            Self::PrivacyClassify | Self::PrivacyRedact => &["privacy_classify_raw"],
            Self::Embeddings => &["embeddings_raw"],
            Self::Rerank => &["rerank"],
            Self::Score => &["score"],
            Self::AudioTranscription => &["audio_transcription"],
            Self::ImageGeneration => &["image_generation", "get_signature"],
            Self::ImageEdit => &["image_edit", "get_signature"],
        }
    }

    /// Whether the model this route needs is created per test rather than
    /// seeded before the suite runs.
    fn test_owned_model(self) -> Option<TestOwnedModel> {
        match self {
            Self::Embeddings => Some(TestOwnedModel::Embedding),
            Self::AudioTranscription => Some(TestOwnedModel::Transcription),
            _ => None,
        }
    }

    fn json_body(self, model: &str) -> Option<serde_json::Value> {
        let body = match self {
            Self::ChatCompletion => serde_json::json!({
                "model": model,
                "stream": false,
                "messages": [{"role": "user", "content": "tenant-chat"}]
            }),
            Self::ChatCompletionStream => serde_json::json!({
                "model": model,
                "stream": true,
                // Continuous usage keeps the streamed bytes unrewritten, so the
                // stream ends with a provider signature fetch rather than a
                // gateway signature.
                "stream_options": {"continuous_usage_stats": true},
                "messages": [{"role": "user", "content": "tenant-stream"}]
            }),
            Self::ChatCompletionAutoRedact => serde_json::json!({
                "model": model,
                "messages": [{
                    "role": "user",
                    "content": "Please reach out to alice@example.com"
                }]
            }),
            Self::TextCompletion => serde_json::json!({
                "model": model,
                "prompt": "Hello",
                "max_tokens": 16
            }),
            Self::Responses => serde_json::json!({
                "model": model,
                "input": "Hi",
                "stream": false,
                "max_output_tokens": 10
            }),
            Self::PrivacyClassify => serde_json::json!({
                "model": model,
                "input": "My SSN is 123-45-6789",
                "threshold": 0.5
            }),
            Self::PrivacyRedact => serde_json::json!({
                "model": model,
                "input": "Email alice@example.com please"
            }),
            Self::Embeddings => serde_json::json!({
                "model": model,
                "input": "Hello world"
            }),
            Self::Rerank => serde_json::json!({
                "model": model,
                "query": "What is the capital of France?",
                "documents": [
                    "The capital of Brazil is Brasilia.",
                    "The capital of France is Paris."
                ]
            }),
            Self::Score => serde_json::json!({
                "model": model,
                "text_1": "What is the capital of France?",
                "text_2": "The capital of France is Paris."
            }),
            Self::ImageGeneration => serde_json::json!({
                "model": model,
                "prompt": "A sunset over mountains",
                "n": 1,
                "response_format": "b64_json"
            }),
            Self::AudioTranscription | Self::ImageEdit => return None,
        };
        Some(body)
    }

    fn seeded_model(self) -> &'static str {
        match self {
            Self::PrivacyClassify | Self::PrivacyRedact => E2E_PRIVACY_FILTER_MODEL_NAME,
            Self::Rerank | Self::Score => E2E_QWEN_RERANKER_MODEL_NAME,
            Self::ImageGeneration | Self::ImageEdit => E2E_QWEN_IMAGE_MODEL_NAME,
            _ => E2E_QWEN_MODEL_NAME,
        }
    }

    async fn send(
        self,
        server: &axum_test::TestServer,
        api_key: &str,
        model: &str,
        client_ids: &ClientIds,
    ) -> axum_test::TestResponse {
        let mut request = server
            .post(self.path())
            .add_header("Authorization", format!("Bearer {api_key}"))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .add_header(REQUEST_ID_HEADER, client_ids.request_id.to_string())
            .add_header(ORG_ID_HEADER, client_ids.org_id.to_string())
            .add_header(WORKSPACE_ID_HEADER, client_ids.workspace_id.to_string());
        if matches!(self, Self::ChatCompletionAutoRedact) {
            request = request.add_header("x-auto-redact", "on");
        }
        if let Some(mut body) = self.json_body(model) {
            // Every JSON route accepts unknown fields, so each also gets the
            // body-level value.
            let fields = body.as_object_mut().expect("request body is an object");
            fields.insert(
                "x_request_id".to_string(),
                client_ids.request_id.to_string().into(),
            );
            fields.insert("x_org_id".to_string(), client_ids.org_id.to_string().into());
            fields.insert(
                "x_workspace_id".to_string(),
                client_ids.workspace_id.to_string().into(),
            );
            return request.json(&body).await;
        }
        let form = match self {
            Self::AudioTranscription => axum_test::multipart::MultipartForm::new()
                .add_part(
                    "file",
                    axum_test::multipart::Part::bytes(vec![0u8; 4 * 1024])
                        .file_name("test.mp3")
                        .mime_type("audio/mpeg"),
                )
                .add_text("model", model.to_string()),
            Self::ImageEdit => axum_test::multipart::MultipartForm::new()
                .add_part(
                    "image",
                    axum_test::multipart::Part::bytes(crate::audio_image::create_test_png_image())
                        .file_name("image.png")
                        .mime_type("image/png"),
                )
                .add_text("model", model.to_string())
                .add_text("prompt", "Change the background to a sunny day"),
            _ => unreachable!("every JSON route has a body"),
        };
        request.multipart(form).await
    }
}

#[derive(Debug, Clone, Copy)]
enum TestOwnedModel {
    Embedding,
    Transcription,
}

/// Create a test-owned model in the catalog and serve it from `mock`.
async fn register_test_owned_model(
    server: &axum_test::TestServer,
    pool: &services::inference_provider_pool::InferenceProviderPool,
    mock: &Arc<MockProvider>,
    kind: TestOwnedModel,
) -> String {
    let (model_name, display_name) = match kind {
        TestOwnedModel::Embedding => (
            format!("test-tenant-ids/embedding-{}", Uuid::new_v4()),
            "Tenant ids embedding fixture",
        ),
        TestOwnedModel::Transcription => (
            format!("test-tenant-ids/transcription-{}", Uuid::new_v4()),
            "Tenant ids transcription fixture",
        ),
    };
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {"amount": 1000000, "currency": "USD"},
            "outputCostPerToken": {"amount": 0, "currency": "USD"},
            "costPerImage": {"amount": 0, "currency": "USD"},
            "modelDisplayName": display_name,
            "modelDescription": "Test-owned model",
            "contextLength": 4096,
            "maxOutputLength": 1024,
            "verifiable": false,
            "isActive": true,
            "inputModalities": ["text"],
            "outputModalities": ["text"]
        }))
        .expect("model fixture should deserialize"),
    );
    seed_models(server, batch, get_session_id()).await;
    let provider: Arc<dyn inference_providers::InferenceProvider + Send + Sync> = mock.clone();
    pool.register_provider(model_name.clone(), provider).await;
    model_name
}

/// Wait until every operation in `expected` appears among the calls recorded
/// since `start` that belong to `org_id` or carry no tenant at all, then return
/// those calls. A call carrying another organization was made by an earlier
/// row's deferred work and is left to the final sweep.
async fn wait_for_row_operations(
    mock: &MockProvider,
    start: usize,
    org_id: Uuid,
    expected: &[&str],
) -> Vec<TracingCall> {
    let deadline = tokio::time::Instant::now() + DEFERRED_CALL_DEADLINE;
    loop {
        let calls: Vec<TracingCall> = mock
            .tracing_calls()
            .into_iter()
            .skip(start)
            .filter(|call| call.context.is_none_or(|context| context.org_id == org_id))
            .collect();
        if expected
            .iter()
            .all(|operation| calls.iter().any(|call| call.operation == *operation))
            || tokio::time::Instant::now() >= deadline
        {
            return calls;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A new self-hosted inference route must carry the authenticated tenant to
/// the provider. The rows share one server and run one after another, each
/// with its own organization: a row looks only at calls recorded after it
/// started that carry its organization or no tenant, waits for its deferred
/// calls before the next row starts, and a final sweep checks every call the
/// mock recorded against the row that owns it. Other tests build their own
/// server and mock, so no other request reaches this mock.
#[tokio::test]
async fn test_request_id_contract_every_inference_route_carries_tenant_ids() {
    allow_debug_attestation_keys();
    let (server, pool, mock_provider, _database) = setup_test_server_with_pool().await;
    let mut row_contexts: Vec<(&str, TracingContext)> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for route in INFERENCE_ROUTES {
        let label = route.label();

        // Given
        let org = setup_org_with_credits(&server, 10_000_000_000).await;
        let workspace_id = list_workspaces(&server, org.id.clone())
            .await
            .first()
            .expect("test org should have a default workspace")
            .id
            .clone();
        let api_key = get_api_key_for_org(&server, org.id.clone()).await;
        let model = match route.test_owned_model() {
            Some(kind) => register_test_owned_model(&server, &pool, &mock_provider, kind).await,
            None => route.seeded_model().to_string(),
        };
        let expected = TenantIds {
            org_id: parse_uuid(&org.id),
            workspace_id: parse_uuid(&workspace_id),
        };
        let client_ids = ClientIds {
            request_id: Uuid::new_v4(),
            org_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
        };
        let start = mock_provider.tracing_calls().len();

        // When
        let response = route.send(&server, &api_key, &model, &client_ids).await;

        // Then
        assert_eq!(
            response.status_code(),
            200,
            "{label}: request should succeed: {}",
            response.text()
        );
        // Drains a streamed body, so work done at the end of the stream runs.
        let _ = response.text();
        let calls = wait_for_row_operations(
            &mock_provider,
            start,
            expected.org_id,
            route.expected_operations(),
        )
        .await;
        println!(
            "request_id_contract {label}: operations={:?}",
            calls.iter().map(|call| call.operation).collect::<Vec<_>>()
        );
        let context = match check_calls_carry_tenant(
            label,
            &calls,
            route.expected_operations(),
            expected,
            client_ids.request_id,
        ) {
            Ok(context) => context,
            Err(failure) => {
                failures.push(failure);
                continue;
            }
        };
        if let Some((other, _)) = row_contexts
            .iter()
            .find(|(_, seen)| seen.request_id == context.request_id)
        {
            panic!("{label}: reused the request id of {other}");
        }
        row_contexts.push((label, context));
        let calls_chat = route
            .expected_operations()
            .iter()
            .any(|operation| operation.starts_with("chat_completion"));
        if calls_chat {
            let params = mock_provider
                .last_chat_params()
                .await
                .expect("chat request should reach mock provider");
            assert_no_client_ids_in_extra(label, &params.extra, context);
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} inference routes did not carry tenant ids:\n{}",
        failures.len(),
        INFERENCE_ROUTES.len(),
        failures.join("\n")
    );

    // Every call recorded, including deferred calls that landed during a
    // later row, carries exactly the context of the row that made it.
    for call in mock_provider.tracing_calls() {
        let context = call
            .context
            .unwrap_or_else(|| panic!("`{}` ran without tenant ids", call.operation));
        let (label, row_context) = row_contexts
            .iter()
            .find(|(_, row)| row.org_id == context.org_id)
            .unwrap_or_else(|| panic!("`{}` carried an unknown organization", call.operation));
        assert_eq!(
            context, *row_context,
            "{label}: `{}` must carry the same ids as the request's other calls",
            call.operation
        );
    }
}

/// Security schemes in the OpenAPI spec that authenticate with an API key.
const API_KEY_SECURITY_SCHEMES: [&str; 2] = ["api_key", "ApiKeyAuth"];

/// Security schemes that do not authenticate with an API key. A scheme that is
/// in neither list fails the guard, so a new one has to be classified.
const OTHER_SECURITY_SCHEMES: [&str; 5] = [
    "session_token",
    "admin_token",
    "refresh_token",
    "organization_credential",
    "reporting_token",
];

/// API-key POST paths under `/v1/` that are not in the inference table, each
/// with the reason. Anything else the spec lists must have a table row.
const NON_INFERENCE_API_KEY_POST_PATHS: [(&str, &str); 4] = [
    (
        "/v1/billing/costs",
        "looks up recorded costs; makes no inference call",
    ),
    (
        "/v1/check_api_key",
        "validates the key; makes no inference call",
    ),
    (
        "/v1/conversations/batch",
        "reads stored conversations; makes no inference call",
    ),
    (
        "/v1/systemone",
        "left out on purpose: whether its backend receives tenant ids is undecided",
    ),
];

/// Inference routes with a table row that the OpenAPI spec does not list.
/// The guard cannot see an unlisted route, so each one is named here; an
/// entry must be dropped once the spec lists the route.
const INFERENCE_POST_PATHS_MISSING_FROM_SPEC: [&str; 1] = ["/v1/embeddings"];

/// A new API-key inference POST route must get a row in `INFERENCE_ROUTES`
/// (or a reasoned entry above) before it can ship. The rule: every POST
/// operation under `/v1/` whose security accepts an API-key scheme is an
/// inference route unless listed in `NON_INFERENCE_API_KEY_POST_PATHS`.
#[tokio::test]
async fn test_request_id_contract_table_covers_every_api_key_inference_route() {
    // Given
    let (server, _) = setup_request_id_server().await;

    // When
    let response = server.get("/api-docs/openapi.json").await;

    // Then
    assert_eq!(response.status_code(), 200);
    let spec = response.json::<serde_json::Value>();
    let schemes = spec["components"]["securitySchemes"]
        .as_object()
        .expect("the spec should define security schemes");
    for name in schemes.keys() {
        assert!(
            API_KEY_SECURITY_SCHEMES.contains(&name.as_str())
                || OTHER_SECURITY_SCHEMES.contains(&name.as_str()),
            "security scheme `{name}` is not classified as API key or not"
        );
    }

    let paths = spec["paths"]
        .as_object()
        .expect("the spec should list paths");
    let mut api_key_posts = std::collections::BTreeSet::new();
    for (path, item) in paths {
        let Some(operation) = item.get("post") else {
            continue;
        };
        let uses_api_key = operation["security"]
            .as_array()
            .is_some_and(|alternatives| {
                alternatives.iter().any(|alternative| {
                    alternative.as_object().is_some_and(|requirement| {
                        requirement
                            .keys()
                            .any(|name| API_KEY_SECURITY_SCHEMES.contains(&name.as_str()))
                    })
                })
            });
        if path.starts_with("/v1/") && uses_api_key {
            api_key_posts.insert(path.as_str());
        }
    }
    for (path, _) in NON_INFERENCE_API_KEY_POST_PATHS {
        assert!(
            api_key_posts.remove(path),
            "excluded path {path} is no longer an API-key POST route; drop it from the list"
        );
    }
    for path in INFERENCE_POST_PATHS_MISSING_FROM_SPEC {
        assert!(
            api_key_posts.insert(path),
            "the spec now lists {path}; drop it from INFERENCE_POST_PATHS_MISSING_FROM_SPEC"
        );
    }
    let covered: std::collections::BTreeSet<&str> =
        INFERENCE_ROUTES.iter().map(|route| route.path()).collect();
    assert_eq!(
        api_key_posts, covered,
        "every API-key inference POST route needs a row in INFERENCE_ROUTES \
         or a reasoned entry in NON_INFERENCE_API_KEY_POST_PATHS"
    );
}
