// E2E tests for per-organization access to unlisted models.
//
// An unlisted model (`isListed: false`) is restricted: an organization may use
// it through this API only while it holds a grant. Everyone else must get
// exactly the answer a model that does not exist gets. Grants are managed by
// admins (`/v1/admin/models/{model_name}/access`) and published to trusted
// services (`GET /v1/internal/model-access`).

use crate::common::{fake_ita::FakeIta, fake_ita::FakeItaMode, *};
use api::models::BatchUpdateModelApiRequest;
use axum::http::Method;
use bytes::Bytes;
use inference_providers::{
    mock::MockProvider, responses_raw::ResponsesRawResponse, SystemOneResponseWithBytes,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

const INTERNAL_TOKEN: &str = "organization-model-access-internal-secret";
const NONCE: &str = "0000000000000000000000000000000000000000000000000000000000000001";
/// Every fixture model has a planned deprecation, so a served request carries
/// the deprecation headers and a refused one must not. `2030-01-01` defaults
/// to 13:00 UTC.
const DEPRECATION_DATE: &str = "2030-01-01";
const DEPRECATION_DATE_HEADER: &str = "2030-01-01T13:00:00Z";
const HEADER_DEPRECATION_DATE: &str = "x-model-deprecation-date";
const HEADER_SUCCESSOR: &str = "x-model-successor";

/// The fixture models, one per kind of route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelKind {
    /// A text model, with an alias.
    Text,
    /// A System One model (output modality `decisions`).
    Decisions,
    /// A model the native Anthropic Messages routes accept.
    Anthropic,
    /// An OpenAI model routed through native Responses.
    NativeResponses,
}

struct Fixture {
    server: axum_test::TestServer,
    /// The router behind `server`, for a test that serves it on a real port.
    router: axum::Router,
    database: Arc<database::Database>,
    pool: Arc<services::inference_provider_pool::InferenceProviderPool>,
    /// Serves every fixture model and records the chat requests it receives.
    provider: Arc<MockProvider>,
    text: String,
    text_alias: String,
    decisions: String,
    anthropic: String,
    native: String,
    /// The listed model every fixture model names as its successor.
    successor: String,
}

impl Fixture {
    fn model(&self, kind: ModelKind) -> &str {
        match kind {
            ModelKind::Text => &self.text,
            ModelKind::Decisions => &self.decisions,
            ModelKind::Anthropic => &self.anthropic,
            ModelKind::NativeResponses => &self.native,
        }
    }
}

/// A name with the shape and length of `model` that no model has. The last 36
/// characters of every fixture name are a UUID.
fn unknown_twin(model: &str) -> String {
    format!("{}{}", &model[..model.len() - 36], Uuid::new_v4())
}

fn model_fields(extra: Value) -> Value {
    let mut fields = json!({
        "inputCostPerToken": { "amount": 1_000_000, "currency": "USD" },
        "outputCostPerToken": { "amount": 2_000_000, "currency": "USD" },
        "modelDisplayName": "Model Access Test Model",
        "modelDescription": "A model used by the model access e2e tests",
        "contextLength": 4096,
        "maxOutputLength": 1024,
        "verifiable": false,
        "isActive": true,
        "isListed": false,
        "inputModalities": ["text"],
        "outputModalities": ["text"]
    });
    for (key, value) in extra.as_object().expect("extra is an object") {
        fields[key] = value.clone();
    }
    fields
}

async fn upsert_models(server: &axum_test::TestServer, models: Vec<(&str, Value)>) {
    let mut batch = BatchUpdateModelApiRequest::new();
    for (name, fields) in models {
        batch.insert(
            name.to_string(),
            serde_json::from_value(fields).expect("valid model update"),
        );
    }
    seed_models(server, batch, get_session_id()).await;
}

/// A server with one unlisted model of each kind, all served by one mock
/// provider. Each has a planned deprecation and names a listed successor.
async fn setup() -> Fixture {
    setup_with(|_| {}).await
}

/// [`setup`] with further configuration.
async fn setup_with(configure: impl FnOnce(&mut config::ApiConfig)) -> Fixture {
    let suffix = Uuid::new_v4();
    let text = format!("access-text/model-{suffix}");
    let text_alias = format!("access-text-alias-{suffix}");
    let decisions = format!("access-decisions/model-{suffix}");
    let anthropic = format!("anthropic/access-model-{suffix}");
    let native = format!("openai/access-native-model-{suffix}");
    let successor = format!("access-successor/model-{suffix}");

    let (server, router, pool, _shared_mock, database) =
        setup_test_server_with_pool_config_and_router(move |config| {
            config.internal_usage_token = Some(INTERNAL_TOKEN.to_string());
            config.external_providers.enable_anthropic_messages = true;
            configure(config);
        })
        .await;

    // The successor first: a model can only name one that exists.
    upsert_models(
        &server,
        vec![(&successor, model_fields(json!({ "isListed": true })))],
    )
    .await;
    let deprecated = |extra: Value| {
        let mut fields = model_fields(extra);
        fields["deprecationDate"] = json!(DEPRECATION_DATE);
        fields["successorModelId"] = json!(successor);
        fields
    };
    upsert_models(
        &server,
        vec![
            (&text, deprecated(json!({ "aliases": [text_alias] }))),
            (
                &decisions,
                deprecated(json!({ "outputModalities": ["decisions"] })),
            ),
            (
                &anthropic,
                deprecated(json!({
                    "providerType": "external",
                    "attestationSupported": false,
                    "providerConfig": {
                        "backend": "anthropic",
                        "base_url": "https://anthropic.invalid/v1"
                    }
                })),
            ),
            (&native, deprecated(json!({ "ownedBy": "openai" }))),
        ],
    )
    .await;

    let provider = Arc::new(
        MockProvider::new_accept_all()
            .with_systemone_handler(|request, _| {
                let raw = serde_json::to_vec(&json!({
                    "id": format!("decision-{}", Uuid::new_v4()),
                    "model": "decision-model",
                    "answers": { "billing": { "type": "noul", "noul": 0.95 } },
                    "usage": { "input_tokens": 10, "output_tokens": 3 }
                }))
                .expect("fixture serializes");
                SystemOneResponseWithBytes::parse(raw, &request)
            })
            .with_responses_handler(|_body: Value| {
                let native = json!({
                    "id": format!("resp_{}", Uuid::new_v4()),
                    "object": "response",
                    "status": "completed",
                    "output": [],
                    "usage": { "input_tokens": 10, "output_tokens": 5, "total_tokens": 15 },
                    "service_tier": "default",
                    "store": false
                });
                ResponsesRawResponse {
                    status: axum::http::StatusCode::OK,
                    headers: [(
                        axum::http::header::CONTENT_TYPE,
                        axum::http::HeaderValue::from_static("application/json"),
                    )]
                    .into_iter()
                    .collect(),
                    body: Box::pin(futures_util::stream::iter(vec![Ok(Bytes::from(
                        native.to_string(),
                    ))])),
                }
            }),
    );
    for model in [&text, &decisions, &anthropic, &native] {
        pool.register_provider(model.clone(), provider.clone())
            .await;
    }

    Fixture {
        server,
        router,
        database,
        pool,
        provider,
        text,
        text_alias,
        decisions,
        anthropic,
        native,
        successor,
    }
}

struct Organization {
    id: String,
    api_key: String,
}

async fn organization(server: &axum_test::TestServer) -> Organization {
    let org = setup_org_with_credits(server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(server, org.id.clone()).await;
    Organization {
        id: org.id,
        api_key,
    }
}

fn access_path(model: &str) -> String {
    format!("/v1/admin/models/{}/access", urlencoding::encode(model))
}

fn grant_path(model: &str, organization_id: &str) -> String {
    format!("{}/{organization_id}", access_path(model))
}

async fn admin_call(
    server: &axum_test::TestServer,
    method: Method,
    path: &str,
    token: &str,
) -> axum_test::TestResponse {
    server
        .method(method, path)
        .add_header("Authorization", format!("Bearer {token}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await
}

async fn grant(server: &axum_test::TestServer, model: &str, organization_id: &str) -> Value {
    let response = admin_call(
        server,
        Method::PUT,
        &grant_path(model, organization_id),
        &get_session_id(),
    )
    .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    response.json()
}

async fn revoke(server: &axum_test::TestServer, model: &str, organization_id: &str) {
    let response = admin_call(
        server,
        Method::DELETE,
        &grant_path(model, organization_id),
        &get_session_id(),
    )
    .await;
    assert_eq!(response.status_code(), 204, "{}", response.text());
}

/// `PATCH /v1/admin/models` for one model.
async fn set_listed(server: &axum_test::TestServer, model: &str, is_listed: bool) {
    upsert_models(server, vec![(model, json!({ "isListed": is_listed }))]).await;
}

/// Every route that takes a model name from the caller and answers for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    ChatCompletions,
    ChatCompletionsStream,
    Completions,
    CompletionsStream,
    Responses,
    ResponsesStream,
    ResponsesNative,
    Messages,
    MessagesCountTokens,
    Embeddings,
    ImageGenerations,
    ImageEdits,
    AudioTranscriptions,
    Rerank,
    Score,
    PrivacyClassify,
    PrivacyRedact,
    SystemOne,
    AttestationReport,
    AttestationReportWithNonce,
}

const ROUTES: &[Route] = &[
    Route::ChatCompletions,
    Route::ChatCompletionsStream,
    Route::Completions,
    Route::CompletionsStream,
    Route::Responses,
    Route::ResponsesStream,
    Route::ResponsesNative,
    Route::Messages,
    Route::MessagesCountTokens,
    Route::Embeddings,
    Route::ImageGenerations,
    Route::ImageEdits,
    Route::AudioTranscriptions,
    Route::Rerank,
    Route::Score,
    Route::PrivacyClassify,
    Route::PrivacyRedact,
    Route::SystemOne,
    Route::AttestationReport,
    Route::AttestationReportWithNonce,
];

impl Route {
    /// The route template as it is registered in the router.
    fn path(self) -> &'static str {
        match self {
            Route::ChatCompletions | Route::ChatCompletionsStream => "/chat/completions",
            Route::Completions | Route::CompletionsStream => "/completions",
            Route::Responses | Route::ResponsesStream | Route::ResponsesNative => "/responses",
            Route::Messages => "/messages",
            Route::MessagesCountTokens => "/messages/count_tokens",
            Route::Embeddings => "/embeddings",
            Route::ImageGenerations => "/images/generations",
            Route::ImageEdits => "/images/edits",
            Route::AudioTranscriptions => "/audio/transcriptions",
            Route::Rerank => "/rerank",
            Route::Score => "/score",
            Route::PrivacyClassify => "/privacy/classify",
            Route::PrivacyRedact => "/privacy/redact",
            Route::SystemOne => "/systemone",
            Route::AttestationReport | Route::AttestationReportWithNonce => "/attestation/report",
        }
    }

    fn model_kind(self) -> ModelKind {
        match self {
            Route::SystemOne => ModelKind::Decisions,
            Route::Messages | Route::MessagesCountTokens => ModelKind::Anthropic,
            Route::ResponsesNative => ModelKind::NativeResponses,
            _ => ModelKind::Text,
        }
    }

    /// The method the route is registered with.
    fn method(self) -> &'static str {
        match self {
            Route::AttestationReport | Route::AttestationReportWithNonce => "GET",
            _ => "POST",
        }
    }

    /// Whether the mock provider can serve the route, so an organization
    /// that holds a grant gets a 200. The native Anthropic routes need a
    /// provider that speaks that protocol, which the mock does not: there a
    /// grant only moves the request past the unknown-model answer.
    fn mock_serves_it(self) -> bool {
        !matches!(self, Route::Messages | Route::MessagesCountTokens)
    }

    /// Whether a served request on this route carries the deprecation
    /// headers of its model. The attestation report is not an inference
    /// route and has no such headers; the Anthropic routes are not served by
    /// the mock.
    fn announces_deprecation(self) -> bool {
        self.mock_serves_it()
            && !matches!(
                self,
                Route::AttestationReport | Route::AttestationReportWithNonce
            )
    }

    /// Whether the route accepts an alias in place of the model's name. The
    /// others look the model up by its canonical name only.
    fn resolves_aliases(self) -> bool {
        matches!(
            self,
            Route::ChatCompletions
                | Route::ChatCompletionsStream
                | Route::Completions
                | Route::CompletionsStream
                | Route::Responses
                | Route::ResponsesStream
                | Route::AudioTranscriptions
                | Route::AttestationReport
                | Route::AttestationReportWithNonce
        )
    }
}

/// The deprecation headers of a response: (date, successor).
fn deprecation_headers(response: &Observed) -> (Option<&str>, Option<&str>) {
    let header = |name: &str| response.headers.get(name).map(String::as_str);
    (header(HEADER_DEPRECATION_DATE), header(HEADER_SUCCESSOR))
}

/// A minimal PNG image (1x1 pixel).
fn png() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ]
}

/// Send the route's request for `model` with `api_key`.
async fn send(
    server: &axum_test::TestServer,
    route: Route,
    api_key: &str,
    model: &str,
) -> axum_test::TestResponse {
    let auth = format!("Bearer {api_key}");
    let post = |path: &str, body: Value| {
        server
            .post(path)
            .add_header("Authorization", auth.clone())
            .json(&body)
    };
    let chat_body = |stream: bool| {
        json!({
            "model": model,
            "messages": [{ "role": "user", "content": "Hello" }],
            "stream": stream,
            "max_tokens": 16
        })
    };
    match route {
        Route::ChatCompletions => post("/v1/chat/completions", chat_body(false)).await,
        Route::ChatCompletionsStream => post("/v1/chat/completions", chat_body(true)).await,
        Route::Completions | Route::CompletionsStream => {
            post(
                "/v1/completions",
                json!({
                    "model": model,
                    "prompt": "Hello",
                    "stream": route == Route::CompletionsStream,
                    "max_tokens": 16
                }),
            )
            .await
        }
        Route::Responses | Route::ResponsesStream => {
            post(
                "/v1/responses",
                json!({
                    "model": model,
                    "input": "Hello",
                    "stream": route == Route::ResponsesStream
                }),
            )
            .await
        }
        Route::ResponsesNative => {
            post(
                "/v1/responses",
                json!({ "model": model, "store": false, "input": "Hello" }),
            )
            .await
        }
        Route::Messages | Route::MessagesCountTokens => {
            let path = if route == Route::Messages {
                "/v1/messages"
            } else {
                "/v1/messages/count_tokens"
            };
            let mut body = json!({
                "model": model,
                "messages": [{ "role": "user", "content": "Hello" }]
            });
            if route == Route::Messages {
                body["max_tokens"] = json!(16);
            }
            post(path, body)
                .add_header("anthropic-version", "2023-06-01")
                .await
        }
        Route::Embeddings => {
            post(
                "/v1/embeddings",
                json!({ "model": model, "input": "Hello world" }),
            )
            .await
        }
        Route::ImageGenerations => {
            post(
                "/v1/images/generations",
                json!({ "model": model, "prompt": "A lighthouse at dusk", "n": 1 }),
            )
            .await
        }
        Route::ImageEdits => {
            server
                .post("/v1/images/edits")
                .add_header("Authorization", auth.clone())
                .multipart(
                    axum_test::multipart::MultipartForm::new()
                        .add_part(
                            "image",
                            axum_test::multipart::Part::bytes(png())
                                .file_name("image.png")
                                .mime_type("image/png"),
                        )
                        .add_text("model", model)
                        .add_text("prompt", "Make the sky clear"),
                )
                .await
        }
        Route::AudioTranscriptions => {
            server
                .post("/v1/audio/transcriptions")
                .add_header("Authorization", auth.clone())
                .multipart(
                    axum_test::multipart::MultipartForm::new()
                        .add_part(
                            "file",
                            axum_test::multipart::Part::bytes(vec![0u8; 1024])
                                .file_name("test.mp3")
                                .mime_type("audio/mpeg"),
                        )
                        .add_text("model", model),
                )
                .await
        }
        Route::Rerank => {
            post(
                "/v1/rerank",
                json!({
                    "model": model,
                    "query": "What is the capital of France?",
                    "documents": ["Paris is the capital of France.", "Cows are animals."]
                }),
            )
            .await
        }
        Route::Score => {
            post(
                "/v1/score",
                json!({ "model": model, "text_1": "Hello world", "text_2": "Hi there" }),
            )
            .await
        }
        Route::PrivacyClassify => {
            post(
                "/v1/privacy/classify",
                json!({ "model": model, "input": "My name is Alice" }),
            )
            .await
        }
        Route::PrivacyRedact => {
            post(
                "/v1/privacy/redact",
                json!({ "model": model, "input": "My name is Alice" }),
            )
            .await
        }
        Route::SystemOne => {
            post(
                "/v1/systemone",
                json!({
                    "model": model,
                    "state": { "message": "Please refund this charge" },
                    "questions": {
                        "billing": { "type": "noul", "instructions": "Is this about billing?" }
                    }
                }),
            )
            .await
        }
        Route::AttestationReport | Route::AttestationReportWithNonce => {
            let model = urlencoding::encode(model);
            let nonce = if route == Route::AttestationReportWithNonce {
                format!("&nonce={NONCE}")
            } else {
                String::new()
            };
            server
                .get(&format!("/v1/attestation/report?model={model}{nonce}"))
                .add_header("Authorization", auth.clone())
                .await
        }
    }
}

/// Everything a client can observe of a response, with the model name it
/// asked for blanked out and the values that differ on every request removed.
#[derive(Debug, PartialEq, Eq)]
struct Observed {
    status: u16,
    headers: BTreeMap<String, String>,
    body: String,
}

fn observe(response: &axum_test::TestResponse, model: &str) -> Observed {
    // Per-request values a client cannot tell two models apart by.
    const PER_REQUEST_HEADERS: &[&str] = &["date", "x-request-id"];
    let headers = response
        .headers()
        .iter()
        .filter(|(name, _)| !PER_REQUEST_HEADERS.contains(&name.as_str()))
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or("<binary>").to_string(),
            )
        })
        .collect();
    Observed {
        status: response.status_code().as_u16(),
        headers,
        body: blank_generated_ids(&response.text().replace(model, "<model>")),
    }
}

/// Blank the ids and timestamps the Responses API generates for every
/// request (`resp_…`, `msg_…`, `"created_at":…`), so two answers to the same
/// request compare equal.
fn blank_generated_ids(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = ["resp_", "msg_", "\"created_at\":"]
        .iter()
        .filter_map(|marker| rest.find(marker).map(|at| (at, *marker)))
        .min()
    {
        let (at, marker) = start;
        out.push_str(&rest[..at + marker.len()]);
        let tail = &rest[at + marker.len()..];
        let generated = tail
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '-')
            .unwrap_or(tail.len());
        out.push('#');
        rest = &tail[generated..];
    }
    out.push_str(rest);
    out
}

/// The enclosing router builder, for resolving a named MethodRouter or a
/// locally nested router. This scanner intentionally supports the source
/// forms used here, not arbitrary Rust; unrecognized route handlers still fail.
fn enclosing_router_builder(code: &str, at: usize) -> (usize, &str) {
    let boundaries = ["\nfn ", "\npub fn "];
    let start = boundaries
        .iter()
        .filter_map(|boundary| code[..at].rfind(boundary))
        .max()
        .unwrap_or(0);
    let end = boundaries
        .iter()
        .filter_map(|boundary| code[at..].find(boundary).map(|end| at + end))
        .min()
        .unwrap_or(code.len());
    (start, &code[start..end])
}

/// Resolve a local `let name = Router::new() ...;` mounted by
/// `.nest("/prefix/", name)` in the same builder. Top-level mounts such as
/// `/v1` stay outside the per-builder inventory, as before.
fn nested_route_path(builder: &str, route_at: usize, path: &str) -> String {
    if let Some(local_at) = builder[..route_at].rfind("let ") {
        let declaration = &builder[local_at + "let ".len()..];
        if let Some((name, expression)) = declaration.split_once('=') {
            let statement_end = declaration.find(';').unwrap_or(declaration.len());
            if local_at + "let ".len() + statement_end > route_at
                && expression.trim_start().starts_with("Router::new()")
            {
                for mount in builder.split(".nest(").skip(1) {
                    let Some((prefix, router)) = mount
                        .trim_start()
                        .strip_prefix('"')
                        .and_then(|mount| mount.split_once('"'))
                    else {
                        continue;
                    };
                    if router.split_once(')').is_some_and(|(router, _)| {
                        router.trim().trim_start_matches(',').trim() == name.trim()
                    }) {
                        return format!("{}{path}", prefix.trim_end_matches('/'));
                    }
                }
            }
        }
    }
    path.to_string()
}

/// Every `.route("<path>", <method>(..))` call in a router source file, as
/// (method, path) pairs. MethodRouter parameters require an `ANY` decision:
/// the model-access inventory covers all methods passed to that path. The
/// Stage I route-layout tests separately check the exact read/delete/410 split.
/// Comment lines and the unit-test module are left out.
fn registered_routes(source: &str) -> Vec<(String, String)> {
    const METHODS: &[&str] = &[
        "get", "post", "put", "patch", "delete", "head", "options", "trace", "any",
    ];
    let production = source
        .split("\n#[cfg(test)]\nmod tests")
        .next()
        .expect("split yields at least one part");
    let code = production
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut routes = Vec::new();
    let mut rest = code.as_str();
    while let Some(at) = rest.find(".route(") {
        let route_at = code.len() - rest.len() + at;
        let (builder_at, builder) = enclosing_router_builder(&code, route_at);
        rest = &rest[at + ".route(".len()..];
        // The arguments of the call: up to the parenthesis that closes it.
        let mut depth = 1usize;
        let end = rest
            .char_indices()
            .find_map(|(index, c)| {
                match c {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                (depth == 0).then_some(index)
            })
            .expect("a `.route(` call that is never closed");
        let call = &rest[..end];
        let quoted = call.trim_start();
        assert!(
            quoted.starts_with('"'),
            "a route path that is not a string literal: {call}"
        );
        let (path, handlers) = quoted[1..]
            .split_once('"')
            .expect("the path's closing quote");

        let mut methods = Vec::new();
        for method in METHODS {
            let token = format!("{method}(");
            let mut from = 0;
            while let Some(found) = handlers[from..].find(&token) {
                let at = from + found;
                // `get(` is a method; `budget(` or `my_get(` is not.
                let starts_a_word = handlers[..at]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
                if starts_a_word {
                    methods.push(method.to_uppercase());
                }
                from = at + token.len();
            }
        }
        if methods.is_empty() {
            let name = handlers
                .trim()
                .trim_start_matches(',')
                .trim()
                .trim_end_matches(',');
            let signature = builder.split('{').next().unwrap_or(builder);
            let parameter = format!("{name}: MethodRouter<");
            assert!(
                signature
                    .lines()
                    .any(|line| line.trim_start().starts_with(&parameter)),
                "no method found for route {path}: {handlers}"
            );
            methods.push("ANY".to_string());
        }
        let path = nested_route_path(builder, route_at - builder_at, path);
        routes.extend(methods.into_iter().map(|method| (method, path.clone())));
        rest = &rest[end..];
    }
    routes
}

/// The access tests cover the routes in `ROUTES`. This one fails when a route
/// is registered in any router builder of `src/lib.rs` (or in the public
/// attestation, credit transfer and organization credential routers it
/// merges) without a decision on record here: either
/// the route is in `ROUTES`, and so in the table test, or it is listed below
/// with the reason it is not. A new route cannot ship without that decision.
///
/// Routes are matched by method and path, as they are registered.
#[test]
fn every_registered_route_has_a_model_access_decision() {
    /// Routes that deal in model names without being inference for an
    /// organization. Each has its own rule, tested elsewhere in this file or
    /// in `unlisted_models.rs`.
    const OTHER_MODEL_ROUTES: &[(&str, &str)] = &[
        // Public catalogs: listed models only.
        ("GET", "/models"),
        ("GET", "/model/list"),
        ("GET", "/model/{model_name}"),
        // Public: resolves as an anonymous caller.
        ("GET", "/attestation/ita-token"),
        // Public: the attested providers' trust pins, published for
        // verifiers. It takes no model name and reads no catalog row.
        ("GET", "/attestation/pins"),
        // Service token: usage is recorded for any model, and the two
        // documents are for a gateway that enforces access itself.
        ("POST", "/internal/usage"),
        ("GET", "/internal/models"),
        ("GET", "/internal/model-access"),
    ];
    /// Routes that take no model name from the caller.
    const NO_MODEL_NAME: &[(&str, &str)] = &[
        ("GET", "/health"),
        ("POST", "/ohttp"),
        ("GET", "/.well-known/ohttp-gateway"),
        ("GET", "/ohttp/config"),
        ("GET", "/docs"),
        ("GET", "/api-docs/openapi.json"),
        // Sign-in and invitations.
        ("POST", "/near"),
        ("GET", "/login"),
        ("GET", "/github"),
        ("GET", "/google"),
        ("GET", "/callback"),
        ("GET", "/user"),
        ("POST", "/logout"),
        ("GET", "/{token}"),
        ("POST", "/{token}/accept"),
        // Retired response history always returns authenticated 410.
        ("ANY", "/responses/{response_id}"),
        ("ANY", "/responses/{response_id}/cancel"),
        ("ANY", "/responses/{response_id}/input_items"),
        // The MCP endpoint serves web search only.
        ("POST", "/mcp"),
        // Stage I migration surfaces take no model name on any method.
        ("ANY", "/conversations"),
        ("ANY", "/conversations/"),
        ("ANY", "/conversations/batch"),
        ("ANY", "/conversations/{conversation_id}"),
        ("ANY", "/conversations/{conversation_id}/"),
        ("ANY", "/conversations/{conversation_id}/items"),
        ("ANY", "/conversations/{conversation_id}/{*legacy_path}"),
        // Looked up by chat id. The `model` query parameter is not read.
        ("GET", "/signature/{chat_id}"),
        ("GET", "/organizations/{org_id}/workspaces"),
        ("POST", "/organizations/{org_id}/workspaces"),
        ("GET", "/workspaces/{workspace_id}"),
        ("PUT", "/workspaces/{workspace_id}"),
        ("DELETE", "/workspaces/{workspace_id}"),
        ("GET", "/workspaces/{workspace_id}/api-keys"),
        ("POST", "/workspaces/{workspace_id}/api-keys"),
        ("DELETE", "/workspaces/{workspace_id}/api-keys/{key_id}"),
        ("PATCH", "/workspaces/{workspace_id}/api-keys/{key_id}"),
        (
            "PATCH",
            "/workspaces/{workspace_id}/api-keys/{key_id}/spend-limit",
        ),
        ("ANY", "/files"),
        ("ANY", "/files/"),
        ("ANY", "/files/{file_id}"),
        ("ANY", "/files/{file_id}/"),
        ("ANY", "/files/{file_id}/content"),
        ("ANY", "/files/{file_id}/{*legacy_path}"),
        ("POST", "/feature-requests"),
        ("POST", "/check_api_key"),
        ("GET", "/services"),
        ("GET", "/services/{service_name}"),
    ];

    let mut registered = registered_routes(include_str!("../../src/lib.rs"));
    // Router builders that `src/lib.rs` calls from other modules.
    registered.extend(registered_routes(include_str!(
        "../../src/routes/attestation.rs"
    )));
    assert!(
        registered.len() >= 100,
        "found too few routes, the scan is broken: {registered:?}"
    );
    let is_registered =
        |method: &str, path: &str| registered.iter().any(|(m, p)| m == method && p == path);

    for (method, path) in &registered {
        // Admin routes act as the platform: their model names are not
        // subject to grants. Their own access rule is the admin permission
        // policy.
        let admin = path.starts_with("/admin/");
        let in_table = ROUTES
            .iter()
            .any(|route| route.method() == method && route.path() == path);
        let listed = [OTHER_MODEL_ROUTES, NO_MODEL_NAME]
            .concat()
            .contains(&(method.as_str(), path.as_str()));
        assert!(
            admin || in_table || listed,
            "{method} {path} has no model access decision. If it takes a model name from \
             the caller, add it to ROUTES (and `send`) so the table test covers it. If not, \
             add it to NO_MODEL_NAME or OTHER_MODEL_ROUTES."
        );
    }

    // The lists must not rot: every entry is still a registered route, and
    // the scan saw every route of the table.
    for (method, path) in OTHER_MODEL_ROUTES.iter().chain(NO_MODEL_NAME) {
        assert!(
            is_registered(method, path),
            "{method} {path} is listed here but no longer registered"
        );
    }
    for route in ROUTES {
        assert!(
            is_registered(route.method(), route.path()),
            "{route:?} is in the table but {} {} is not registered",
            route.method(),
            route.path()
        );
    }
}

/// The scanner itself: methods chained on one path, a layered handler, and
/// names that only look like methods.
#[test]
fn registered_routes_reads_methods_and_paths() {
    let source = r#"
        Router::new()
            .route("/a", get(get_a).delete(delete_a))
            .route(
                "/b/{id}",
                post(budget(my_get)).layer(DefaultBodyLimit::max(LIMIT)),
            )
            // .route("/commented-out", get(gone))
            .route("/c", axum::routing::patch(update_c))

#[cfg(test)]
mod tests {
    fn app() -> Router { Router::new().route("/test-only", get(ok)) }
}
"#;
    let routes = registered_routes(source);
    let routes: Vec<(&str, &str)> = routes
        .iter()
        .map(|(method, path)| (method.as_str(), path.as_str()))
        .collect();
    assert_eq!(
        routes,
        vec![
            ("GET", "/a"),
            ("DELETE", "/a"),
            ("POST", "/b/{id}"),
            ("PATCH", "/c"),
        ]
    );
}

#[test]
fn registered_routes_reads_nested_method_router_parameters() {
    let source = r#"
fn layout<S>(
    item: MethodRouter<S>,
) -> Router<S> {
    let descendants = Router::new()
        .route("/", any(gone))
        .route("/{id}", item);
    Router::new()
        .route("/items", get(list))
        .nest("/items/", descendants)
}
fn other_layout<S>(
    item: MethodRouter<S>,
) -> Router<S> {
    let descendants = Router::new().route("/{id}", item);
    Router::new().nest("/other/", descendants)
}
"#;
    assert_eq!(
        registered_routes(source),
        vec![
            ("ANY".to_string(), "/items/".to_string()),
            ("ANY".to_string(), "/items/{id}".to_string()),
            ("GET".to_string(), "/items".to_string()),
            ("ANY".to_string(), "/other/{id}".to_string()),
        ]
    );
}

#[test]
#[should_panic(expected = "no method found for route /unknown")]
fn registered_routes_rejects_unknown_named_handlers() {
    registered_routes(
        r#"
fn layout<S>(
    known: MethodRouter<S>,
) -> Router<S> {
    Router::new().route("/unknown", unknown)
}
"#,
    );
}

/// (a) For an unlisted model that has a provider, an organization without a
/// grant gets on every route exactly what an unknown model gets. With a grant
/// the request passes the check.
#[tokio::test]
async fn unlisted_model_is_unknown_without_a_grant_on_every_inference_route() {
    let fixture = setup().await;
    let server = &fixture.server;
    let outsider = organization(server).await;
    let member = organization(server).await;
    for kind in [
        ModelKind::Text,
        ModelKind::Decisions,
        ModelKind::Anthropic,
        ModelKind::NativeResponses,
    ] {
        grant(server, fixture.model(kind), &member.id).await;
    }

    for &route in ROUTES {
        let model = fixture.model(route.model_kind());
        let unknown = unknown_twin(model);

        let missing = send(server, route, &outsider.api_key, &unknown).await;
        let refused = send(server, route, &outsider.api_key, model).await;
        let missing = observe(&missing, &unknown);
        let refused = observe(&refused, model);
        // A streaming Responses request answers 200 and fails in the stream.
        assert!(
            (400..500).contains(&missing.status) || missing.body.contains("event: response.failed"),
            "{route:?}: an unknown model must be an error: {missing:?}"
        );
        assert_eq!(
            refused, missing,
            "{route:?}: an unlisted model without a grant must look unknown"
        );

        let granted = send(server, route, &member.api_key, model).await;
        if route.mock_serves_it() {
            assert_eq!(
                granted.status_code(),
                200,
                "{route:?}: the granted organization must be served: {}",
                granted.text()
            );
        }
        let granted = observe(&granted, model);
        assert_ne!(
            granted, refused,
            "{route:?}: a grant must move the request past the unknown-model answer"
        );
        assert!(
            !granted.body.contains("event: response.failed"),
            "{route:?}: the granted stream must not fail: {granted:?}"
        );

        // The model has a planned deprecation. Its headers are part of what
        // was compared above; spell the two cases out.
        assert_eq!(deprecation_headers(&refused), (None, None), "{route:?}");
        if route.announces_deprecation() {
            assert_eq!(
                deprecation_headers(&granted),
                (
                    Some(DEPRECATION_DATE_HEADER),
                    Some(fixture.successor.as_str())
                ),
                "{route:?}"
            );
        }

        // The same through the model's alias. A route that does not resolve
        // aliases answers "unknown" to everyone.
        if route.model_kind() != ModelKind::Text {
            continue;
        }
        let alias = &fixture.text_alias;
        let unknown_alias = unknown_twin(alias);
        let missing = observe(
            &send(server, route, &outsider.api_key, &unknown_alias).await,
            &unknown_alias,
        );
        let refused = observe(&send(server, route, &outsider.api_key, alias).await, alias);
        assert_eq!(
            refused, missing,
            "{route:?}: the alias of an unlisted model without a grant must look unknown"
        );
        assert!(
            !refused.body.contains(model),
            "{route:?}: the alias answer names the canonical model: {refused:?}"
        );
        let granted = observe(&send(server, route, &member.api_key, alias).await, alias);
        if route.resolves_aliases() {
            assert_ne!(granted, refused, "{route:?} by alias");
            // The audio route resolves the alias but then hands the alias
            // itself to the provider pool, which does not know it. That is
            // how it behaves for a listed model too; the request is past
            // the access check either way.
            if route != Route::AudioTranscriptions {
                assert_eq!(granted.status, 200, "{route:?} by alias: {granted:?}");
            }
        } else {
            assert_eq!(granted, refused, "{route:?} does not resolve aliases");
        }
    }
}

/// A planned deprecation is announced in response headers. For an unlisted
/// model they would tell a caller without a grant that the model exists, on
/// every route, and above all on the streaming Responses route, which
/// answers 200 before the model is resolved.
#[tokio::test]
async fn deprecation_headers_are_only_added_for_a_caller_that_may_use_the_model() {
    let fixture = setup().await;
    let server = &fixture.server;
    let outsider = organization(server).await;
    let member = organization(server).await;
    for kind in [
        ModelKind::Text,
        ModelKind::Decisions,
        ModelKind::Anthropic,
        ModelKind::NativeResponses,
    ] {
        grant(server, fixture.model(kind), &member.id).await;
    }
    let announced = (
        Some(DEPRECATION_DATE_HEADER),
        Some(fixture.successor.as_str()),
    );

    for &route in ROUTES {
        let model = fixture.model(route.model_kind());

        let refused = send(server, route, &outsider.api_key, model).await;
        let refused = observe(&refused, model);
        assert_eq!(
            deprecation_headers(&refused),
            (None, None),
            "{route:?}: headers for a model the caller may not use: {refused:?}"
        );
        // The successor is not named anywhere else in the answer either.
        assert!(
            !refused.body.contains(&fixture.successor),
            "{route:?}: {refused:?}"
        );

        let granted = observe(&send(server, route, &member.api_key, model).await, model);
        if route.announces_deprecation() {
            assert_eq!(deprecation_headers(&granted), announced, "{route:?}");
        }
    }

    // The streaming Responses route is the one that answers 200 either way.
    let refused = send(
        server,
        Route::ResponsesStream,
        &outsider.api_key,
        &fixture.text,
    )
    .await;
    assert_eq!(refused.status_code(), 200);
    assert!(refused.text().contains("event: response.failed"));
    assert!(refused.maybe_header(HEADER_DEPRECATION_DATE).is_none());
    assert!(refused.maybe_header(HEADER_SUCCESSOR).is_none());

    // By alias: nothing without a grant, the canonical model's headers with
    // one.
    for (org, expected) in [(&outsider, (None, None)), (&member, announced)] {
        let response = send(
            server,
            Route::ResponsesStream,
            &org.api_key,
            &fixture.text_alias,
        )
        .await;
        let response = observe(&response, &fixture.text_alias);
        assert_eq!(deprecation_headers(&response), expected);
    }

    // Revoking takes the headers away with the access.
    revoke(server, &fixture.text, &member.id).await;
    let revoked = send(
        server,
        Route::ResponsesStream,
        &member.api_key,
        &fixture.text,
    )
    .await;
    assert!(revoked.maybe_header(HEADER_DEPRECATION_DATE).is_none());
    assert!(revoked.maybe_header(HEADER_SUCCESSOR).is_none());
}

/// A listed model may name an unlisted successor. Its name reaches only a
/// caller that may use it: the deprecation header carries it for an
/// organization that holds a grant for the successor, and the public
/// catalogs leave it out until the successor is listed.
#[tokio::test]
async fn an_unlisted_successor_is_named_only_to_a_caller_that_may_use_it() {
    let fixture = setup().await;
    let server = &fixture.server;
    let suffix = Uuid::new_v4();
    let successor = format!("access-restricted-successor/model-{suffix}");
    let public = format!("access-public/model-{suffix}");
    upsert_models(server, vec![(&successor, model_fields(json!({})))]).await;
    let mut fields = model_fields(json!({ "isListed": true }));
    fields["deprecationDate"] = json!(DEPRECATION_DATE);
    fields["successorModelId"] = json!(successor);
    upsert_models(server, vec![(&public, fields)]).await;
    fixture
        .pool
        .register_provider(public.clone(), fixture.provider.clone())
        .await;
    let outsider = organization(server).await;
    let member = organization(server).await;
    grant(server, &successor, &member.id).await;

    let headers = |org: &Organization| {
        let api_key = org.api_key.clone();
        let public = public.clone();
        async move {
            let response = send(server, Route::ChatCompletions, &api_key, &public).await;
            assert_eq!(response.status_code(), 200, "{}", response.text());
            let observed = observe(&response, &public);
            (
                deprecation_headers(&observed).0.map(str::to_string),
                deprecation_headers(&observed).1.map(str::to_string),
                observed.body,
            )
        }
    };
    let catalogs = || async {
        let mut bodies = Vec::new();
        for path in [
            "/v1/models".to_string(),
            "/v1/model/list?limit=100000".to_string(),
            format!("/v1/model/{}", urlencoding::encode(&public)),
        ] {
            let response = server.get(&path).await;
            assert_eq!(response.status_code(), 200, "{path}");
            assert!(response.text().contains(&public), "{path}");
            bodies.push((path, response.text()));
        }
        bodies
    };

    let (date, named, body) = headers(&outsider).await;
    assert_eq!(date.as_deref(), Some(DEPRECATION_DATE_HEADER));
    assert_eq!(named, None);
    assert!(!body.contains(&successor), "{body}");
    let (_, named, _) = headers(&member).await;
    assert_eq!(named.as_deref(), Some(successor.as_str()));
    for (path, body) in catalogs().await {
        assert!(!body.contains(&successor), "{path}: {body}");
    }

    // Once the successor is listed, everyone may use it and see it.
    set_listed(server, &successor, true).await;
    let (_, named, _) = headers(&outsider).await;
    assert_eq!(named.as_deref(), Some(successor.as_str()));
    for (path, body) in catalogs().await {
        assert!(body.contains(&successor), "{path}: {body}");
    }
}

/// The pre-dispatch checks of the chat routes answer for the requested model
/// before the completion service resolves it. None of them may answer for an
/// unlisted model the organization holds no grant for.
#[tokio::test]
async fn pre_dispatch_checks_do_not_disclose_an_unlisted_model() {
    let fixture = setup().await;
    let server = &fixture.server;
    let outsider = organization(server).await;
    let unknown = unknown_twin(&fixture.text);
    let unknown_alias = unknown_twin(&fixture.text_alias);

    // A video part is refused with the model's declared modalities.
    let video = |model: &str| {
        server
            .post("/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {}", outsider.api_key))
            .json(&json!({
                "model": model,
                "messages": [{ "role": "user", "content": [
                    { "type": "text", "text": "Describe this" },
                    { "type": "video_url", "video_url": { "url": "https://example.com/clip.mp4" } }
                ]}],
                "max_tokens": 16
            }))
    };
    let refused = video(&fixture.text).await;
    assert!(
        !refused.text().contains("does not support"),
        "the modality check answered for a model the caller may not use: {}",
        refused.text()
    );
    assert_eq!(
        observe(&refused, &fixture.text),
        observe(&video(&unknown).await, &unknown)
    );

    // Strict alias mode names the canonical model of an alias.
    for path in ["/v1/chat/completions", "/v1/completions"] {
        let strict = |model: &str| {
            let body = if path == "/v1/completions" {
                json!({ "model": model, "prompt": "Hello", "max_tokens": 16 })
            } else {
                json!({
                    "model": model,
                    "messages": [{ "role": "user", "content": "Hello" }],
                    "max_tokens": 16
                })
            };
            server
                .post(path)
                .add_header("Authorization", format!("Bearer {}", outsider.api_key))
                .add_header("x-no-aliasing", "true")
                .json(&body)
        };
        let refused = strict(&fixture.text_alias).await;
        assert!(
            !refused.text().contains(&fixture.text),
            "{path}: the alias check named the canonical model: {}",
            refused.text()
        );
        assert_eq!(
            observe(&refused, &fixture.text_alias),
            observe(&strict(&unknown_alias).await, &unknown_alias),
            "{path}"
        );
    }

    // The attestation report resolves aliases too, and announces them.
    let report = |model: &str, strict: bool| {
        let mut request = server
            .get(&format!(
                "/v1/attestation/report?model={}",
                urlencoding::encode(model)
            ))
            .add_header("Authorization", format!("Bearer {}", outsider.api_key));
        if strict {
            request = request.add_header("x-no-aliasing", "true");
        }
        request
    };
    for strict in [false, true] {
        let refused = report(&fixture.text_alias, strict).await;
        assert!(
            !refused.text().contains(&fixture.text),
            "the report route named the canonical model: {}",
            refused.text()
        );
        assert_eq!(
            observe(&refused, &fixture.text_alias),
            observe(&report(&unknown_alias, strict).await, &unknown_alias)
        );
    }

    // None of this reached the provider.
    assert!(fixture.provider.last_chat_params().await.is_none());
}

/// (b) Granting and revoking take effect at once on the instance that served
/// the admin request, also when the answer for the organization was cached.
#[tokio::test]
async fn grant_and_revoke_take_effect_at_once() {
    let fixture = setup().await;
    let server = &fixture.server;
    let org = organization(server).await;
    let other = organization(server).await;
    let chat = || send(server, Route::ChatCompletions, &org.api_key, &fixture.text);
    let embed = || send(server, Route::Embeddings, &org.api_key, &fixture.text);
    let unknown = unknown_twin(&fixture.text);
    let unknown_chat = observe(
        &send(server, Route::ChatCompletions, &org.api_key, &unknown).await,
        &unknown,
    );

    // Refused, twice: the second answer comes from the cached decision.
    for _ in 0..2 {
        assert_eq!(observe(&chat().await, &fixture.text), unknown_chat);
        assert_eq!(embed().await.status_code(), 404);
    }

    grant(server, &fixture.text, &org.id).await;
    for _ in 0..2 {
        assert_eq!(chat().await.status_code(), 200);
        assert_eq!(embed().await.status_code(), 200);
        // The alias resolves for the organization as well.
        let aliased = send(
            server,
            Route::ChatCompletions,
            &org.api_key,
            &fixture.text_alias,
        )
        .await;
        assert_eq!(aliased.status_code(), 200, "{}", aliased.text());
    }
    // The grant is the organization's, nobody else's.
    assert_eq!(
        send(
            server,
            Route::ChatCompletions,
            &other.api_key,
            &fixture.text
        )
        .await
        .status_code(),
        unknown_chat.status
    );

    revoke(server, &fixture.text, &org.id).await;
    for _ in 0..2 {
        assert_eq!(observe(&chat().await, &fixture.text), unknown_chat);
        assert_eq!(embed().await.status_code(), 404);
    }
}

/// (g) Listing decides. A listed model is open to everyone and grants on it
/// have no effect; unlisting it restricts it at once, and the grants prepared
/// while it was listed apply from that moment.
#[tokio::test]
async fn listing_a_model_opens_it_and_unlisting_restricts_it() {
    let fixture = setup().await;
    let server = &fixture.server;
    let prepared = organization(server).await;
    let outsider = organization(server).await;
    let chat = |org: &Organization| {
        let api_key = org.api_key.clone();
        let model = fixture.text.clone();
        async move {
            send(server, Route::ChatCompletions, &api_key, &model)
                .await
                .status_code()
                .as_u16()
        }
    };

    set_listed(server, &fixture.text, true).await;
    assert_eq!(chat(&outsider).await, 200);
    assert_eq!(chat(&prepared).await, 200);

    // Prepare access while the model is still open to everyone.
    grant(server, &fixture.text, &prepared.id).await;
    assert_eq!(chat(&outsider).await, 200);

    set_listed(server, &fixture.text, false).await;
    assert_eq!(chat(&prepared).await, 200);
    assert_ne!(chat(&outsider).await, 200);

    set_listed(server, &fixture.text, true).await;
    assert_eq!(chat(&outsider).await, 200);
}

/// A grant counts only while the model is active.
#[tokio::test]
async fn a_grant_on_an_inactive_model_has_no_effect() {
    let fixture = setup().await;
    let server = &fixture.server;
    let org = organization(server).await;
    grant(server, &fixture.text, &org.id).await;
    assert_eq!(
        send(server, Route::Embeddings, &org.api_key, &fixture.text)
            .await
            .status_code(),
        200
    );

    upsert_models(server, vec![(&fixture.text, json!({ "isActive": false }))]).await;
    assert_eq!(
        send(server, Route::Embeddings, &org.api_key, &fixture.text)
            .await
            .status_code(),
        404
    );
    // The grant is still there for the admin, with the reason it is idle.
    let access: Value = admin_call(
        server,
        Method::GET,
        &access_path(&fixture.text),
        &get_session_id(),
    )
    .await
    .json();
    assert_eq!(access["isActive"], false);
    assert_eq!(access["organizations"][0]["organizationId"], org.id);
    // The gateway document keeps the model, with its grants. A lane may
    // still be serving it, and a model that is missing from the document is
    // read as unrestricted.
    assert_eq!(
        internal_entry(server, &fixture.text).await,
        Some(json!({ "id": fixture.text, "organization_ids": [org.id] }))
    );

    // Grants on an inactive model can still be managed, so it can be
    // prepared before it is activated and cleaned up after.
    let other = organization(server).await;
    grant(server, &fixture.text, &other.id).await;
    revoke(server, &fixture.text, &org.id).await;
    upsert_models(server, vec![(&fixture.text, json!({ "isActive": true }))]).await;
    // Deactivating a model unregisters its provider.
    fixture
        .pool
        .register_provider(fixture.text.clone(), fixture.provider.clone())
        .await;
    assert_eq!(
        send(server, Route::Embeddings, &other.api_key, &fixture.text)
            .await
            .status_code(),
        200
    );
    assert_eq!(
        send(server, Route::Embeddings, &org.api_key, &fixture.text)
            .await
            .status_code(),
        404
    );
}

/// The audit trail of one organization's access to one model, oldest first:
/// (action, acting user).
async fn audit_trail(
    fixture: &Fixture,
    model: &str,
    organization_id: &str,
) -> Vec<(String, Option<Uuid>)> {
    fixture
        .database
        .pool()
        .get()
        .await
        .expect("database connection")
        .query(
            "SELECT l.action, l.acting_user_id \
             FROM organization_model_access_log l \
             JOIN models m ON m.id = l.model_id \
             WHERE m.model_name = $1 AND l.organization_id = $2 \
             ORDER BY l.created_at, l.action",
            &[
                &model,
                &Uuid::parse_str(organization_id).expect("organization id"),
            ],
        )
        .await
        .expect("read the audit trail")
        .iter()
        .map(|row| (row.get("action"), row.get("acting_user_id")))
        .collect()
}

/// Every grant and every revocation leaves one audit row, with the admin who
/// made it. A call that changes nothing leaves none.
#[tokio::test]
async fn grants_and_revocations_are_audited() {
    let fixture = setup().await;
    let server = &fixture.server;
    let org = organization(server).await;
    let other = organization(server).await;
    let model = &fixture.text;
    let admin = Some(Uuid::parse_str(MOCK_USER_ID).expect("mock user id"));
    let granted = ("grant".to_string(), admin);
    let revoked = ("revoke".to_string(), admin);

    assert_eq!(audit_trail(&fixture, model, &org.id).await, vec![]);

    // Revoking a grant that does not exist changes nothing.
    revoke(server, model, &org.id).await;
    assert_eq!(audit_trail(&fixture, model, &org.id).await, vec![]);

    // One row for the grant, none for granting it again.
    grant(server, model, &org.id).await;
    grant(server, model, &org.id).await;
    assert_eq!(
        audit_trail(&fixture, model, &org.id).await,
        vec![granted.clone()]
    );

    // One row for the revocation, none for revoking again. The grant row
    // is gone from the grants table; the trail still tells the story.
    revoke(server, model, &org.id).await;
    revoke(server, model, &org.id).await;
    assert_eq!(
        audit_trail(&fixture, model, &org.id).await,
        vec![granted.clone(), revoked.clone()]
    );

    // Granting again is a new event.
    grant(server, model, &org.id).await;
    assert_eq!(
        audit_trail(&fixture, model, &org.id).await,
        vec![granted.clone(), revoked, granted.clone()]
    );

    // The trail is per organization and per model.
    assert_eq!(audit_trail(&fixture, model, &other.id).await, vec![]);
    assert_eq!(
        audit_trail(&fixture, &fixture.decisions, &org.id).await,
        vec![]
    );

    // A request that is refused writes nothing.
    let unknown_org = Uuid::new_v4().to_string();
    let refused = admin_call(
        server,
        Method::PUT,
        &grant_path(model, &unknown_org),
        &get_session_id(),
    )
    .await;
    assert_eq!(refused.status_code(), 404);
    assert_eq!(audit_trail(&fixture, model, &unknown_org).await, vec![]);

    // Deleting the organization removes its grant: a revocation by the
    // platform, with no acting user.
    let deleted = server
        .delete(&format!("/v1/organizations/{}", org.id))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert!(
        deleted.status_code().is_success(),
        "deleting the organization failed: {}",
        deleted.text()
    );
    let trail = audit_trail(&fixture, model, &org.id).await;
    assert_eq!(trail.len(), 4, "{trail:?}");
    assert_eq!(trail[3], ("revoke".to_string(), None));
}

/// (c) The admin routes: happy paths, idempotency and 404s.
#[tokio::test]
async fn admin_manages_grants() {
    let fixture = setup().await;
    let server = &fixture.server;
    let session = get_session_id();
    let first = organization(server).await;
    let second = organization(server).await;
    let model = &fixture.text;

    // Nobody holds a grant yet.
    let access = admin_call(server, Method::GET, &access_path(model), &session).await;
    assert_eq!(access.status_code(), 200, "{}", access.text());
    let access: Value = access.json();
    assert_eq!(access["modelName"], model.as_str());
    assert_eq!(access["isListed"], false);
    assert_eq!(access["isActive"], true);
    assert!(Uuid::parse_str(access["id"].as_str().expect("model id")).is_ok());
    assert_eq!(access["organizations"], json!([]));

    // Grant: the answer is the stored grant, with who made it.
    let granted = grant(server, model, &first.id).await;
    assert_eq!(granted["organizationId"], first.id);
    assert!(granted["organizationName"].is_string());
    assert_eq!(granted["grantedByUserId"], MOCK_USER_ID);
    assert_eq!(granted["grantedByUserEmail"], "admin@test.com");
    let granted_at = granted["grantedAt"]
        .as_str()
        .expect("grantedAt")
        .to_string();

    // Idempotent: a second PUT returns the same grant, not a newer one.
    let again = grant(server, model, &first.id).await;
    assert_eq!(again, granted);

    grant(server, model, &second.id).await;
    let access: Value = admin_call(server, Method::GET, &access_path(model), &session)
        .await
        .json();
    let holders: Vec<&str> = access["organizations"]
        .as_array()
        .expect("organizations")
        .iter()
        .map(|grant| grant["organizationId"].as_str().expect("organizationId"))
        .collect();
    assert_eq!(holders, vec![first.id.as_str(), second.id.as_str()]);
    assert_eq!(access["organizations"][0]["grantedAt"], granted_at.as_str());

    // The organization's side of the same grants.
    let path = format!("/v1/admin/organizations/{}/model-access", first.id);
    let of_first = admin_call(server, Method::GET, &path, &session).await;
    assert_eq!(of_first.status_code(), 200, "{}", of_first.text());
    let of_first: Value = of_first.json();
    assert_eq!(of_first["organizationId"], first.id);
    assert_eq!(
        of_first["models"],
        json!([{
            "id": access["id"],
            "modelName": model,
            "isListed": false,
            "isActive": true,
            "grantedAt": granted_at,
            "grantedByUserId": MOCK_USER_ID,
            "grantedByUserEmail": "admin@test.com"
        }])
    );

    // Revoke, and revoke again: both succeed.
    revoke(server, model, &first.id).await;
    revoke(server, model, &first.id).await;
    let access: Value = admin_call(server, Method::GET, &access_path(model), &session)
        .await
        .json();
    assert_eq!(access["organizations"].as_array().unwrap().len(), 1);
    assert_eq!(access["organizations"][0]["organizationId"], second.id);
    let of_first: Value = admin_call(server, Method::GET, &path, &session)
        .await
        .json();
    assert_eq!(of_first["models"], json!([]));

    // Unknown model or organization: 404, and nothing is written.
    let missing_model = unknown_twin(model);
    let missing_org = Uuid::new_v4().to_string();
    for (method, path, error_type) in [
        (Method::GET, access_path(&missing_model), "model_not_found"),
        (
            Method::PUT,
            grant_path(&missing_model, &first.id),
            "model_not_found",
        ),
        (
            Method::DELETE,
            grant_path(&missing_model, &first.id),
            "model_not_found",
        ),
        (
            Method::PUT,
            grant_path(model, &missing_org),
            "organization_not_found",
        ),
        (
            Method::DELETE,
            grant_path(model, &missing_org),
            "organization_not_found",
        ),
        (
            Method::GET,
            format!("/v1/admin/organizations/{missing_org}/model-access"),
            "organization_not_found",
        ),
    ] {
        let response = admin_call(server, method.clone(), &path, &session).await;
        assert_eq!(response.status_code(), 404, "{method} {path}");
        assert_eq!(
            response.json::<Value>()["error"]["type"],
            error_type,
            "{method} {path}"
        );
    }
    let malformed = admin_call(
        server,
        Method::PUT,
        &grant_path(model, "not-a-uuid"),
        &session,
    )
    .await;
    assert_eq!(malformed.status_code(), 400);
    assert_eq!(malformed.json::<Value>()["error"]["type"], "invalid_id");

    // A model name with a slash that is not URL-encoded is not a route.
    let unencoded = admin_call(
        server,
        Method::GET,
        &format!("/v1/admin/models/{model}/access"),
        &session,
    )
    .await;
    assert_eq!(unencoded.status_code(), 404);
}

/// (c) Permissions: the routes need an admin, a read-only admin token can
/// read grants but not change them.
#[tokio::test]
async fn admin_routes_follow_the_admin_token_permissions() {
    let suffix = Uuid::new_v4();
    let model = format!("access-permissions/model-{suffix}");
    let (server, database) = setup_test_server_with_config_and_database(|config| {
        config.auth.admin_read_only_tokens_enabled = true;
    })
    .await;
    upsert_models(&server, vec![(&model, model_fields(json!({})))]).await;
    let org = organization(&server).await;
    let listing = access_path(&model);
    let of_org = format!("/v1/admin/organizations/{}/model-access", org.id);
    let one_grant = grant_path(&model, &org.id);

    let repo = database::repositories::AdminAccessTokenRepository::new(database.pool().clone());
    let mut tokens = Vec::new();
    for permission in [
        database::models::AdminAccessTokenPermission::ReadOnly,
        database::models::AdminAccessTokenPermission::ReadWrite,
    ] {
        let (_, token) = repo
            .create(
                Uuid::parse_str(MOCK_USER_ID).unwrap(),
                format!("model-access-{suffix}"),
                "model access permission test".into(),
                chrono::Utc::now() + chrono::Duration::hours(1),
                Some(MOCK_USER_AGENT.into()),
                permission,
            )
            .await
            .unwrap();
        tokens.push(token);
    }
    let (read_only, read_write) = (&tokens[0], &tokens[1]);

    // No credentials, no answer.
    for (method, path) in [
        (Method::GET, &listing),
        (Method::GET, &of_org),
        (Method::PUT, &one_grant),
        (Method::DELETE, &one_grant),
    ] {
        let anonymous = server.method(method.clone(), path).await;
        assert_eq!(anonymous.status_code(), 401, "{method} {path}");
    }

    // Read-only: both listings, no writes.
    for path in [&listing, &of_org] {
        let response = admin_call(&server, Method::GET, path, read_only).await;
        assert_eq!(response.status_code(), 200, "{path}: {}", response.text());
    }
    for method in [Method::PUT, Method::DELETE] {
        let response = admin_call(&server, method.clone(), &one_grant, read_only).await;
        assert_eq!(response.status_code(), 403, "{method}");
        assert_eq!(
            response.json::<Value>()["error"]["type"],
            "insufficient_permissions"
        );
    }
    let access: Value = admin_call(&server, Method::GET, &listing, read_only)
        .await
        .json();
    assert_eq!(access["organizations"], json!([]));

    // Read-write: grant and revoke.
    let granted = admin_call(&server, Method::PUT, &one_grant, read_write).await;
    assert_eq!(granted.status_code(), 200, "{}", granted.text());
    assert_eq!(granted.json::<Value>()["grantedByUserId"], MOCK_USER_ID);
    let revoked = admin_call(&server, Method::DELETE, &one_grant, read_write).await;
    assert_eq!(revoked.status_code(), 204);
}

async fn internal_document(server: &axum_test::TestServer) -> Value {
    let response = server
        .get("/v1/internal/model-access")
        .add_header("Authorization", format!("Bearer {INTERNAL_TOKEN}"))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(response.header("cache-control"), "no-store");
    let document: Value = response.json();
    assert_eq!(document["object"], "model_access");
    document
}

/// The entry of `model` in `GET /v1/internal/model-access`.
async fn internal_entry(server: &axum_test::TestServer, model: &str) -> Option<Value> {
    internal_document(server).await["models"]
        .as_array()
        .expect("models is an array")
        .iter()
        .find(|entry| entry["id"] == model)
        .cloned()
}

fn sorted(mut ids: Vec<&str>) -> Vec<&str> {
    ids.sort_unstable();
    ids
}

/// (d) `GET /v1/internal/model-access`: every unlisted model, active or not,
/// with the active organizations that hold a grant.
#[tokio::test]
async fn internal_document_lists_restricted_models_and_their_organizations() {
    let fixture = setup().await;
    let server = &fixture.server;
    let first = organization(server).await;
    let second = organization(server).await;
    let model = &fixture.text;

    // Restricted, and nobody may use it: listed with no organizations.
    assert_eq!(
        internal_entry(server, model).await,
        Some(json!({ "id": model, "organization_ids": [] }))
    );

    grant(server, model, &first.id).await;
    grant(server, model, &second.id).await;
    let entry = internal_entry(server, model).await.expect("restricted");
    assert_eq!(
        entry.as_object().expect("entry is an object").len(),
        2,
        "the entry carries id and organization_ids only: {entry}"
    );
    let ids: Vec<&str> = entry["organization_ids"]
        .as_array()
        .expect("organization_ids")
        .iter()
        .map(|id| id.as_str().expect("an organization id"))
        .collect();
    assert_eq!(ids, sorted(vec![first.id.as_str(), second.id.as_str()]));

    revoke(server, model, &first.id).await;
    assert_eq!(
        internal_entry(server, model).await,
        Some(json!({ "id": model, "organization_ids": [second.id] }))
    );

    // A deleted organization no longer counts, and its grant is gone.
    let deleted = server
        .delete(&format!("/v1/organizations/{}", second.id))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert!(
        deleted.status_code().is_success(),
        "deleting the organization failed: {}",
        deleted.text()
    );
    assert_eq!(
        internal_entry(server, model).await,
        Some(json!({ "id": model, "organization_ids": [] }))
    );
    let access: Value = admin_call(server, Method::GET, &access_path(model), &get_session_id())
        .await
        .json();
    assert_eq!(access["organizations"], json!([]));
    let rows: i64 = fixture
        .database
        .pool()
        .get()
        .await
        .expect("database connection")
        .query_one(
            "SELECT COUNT(*) FROM organization_model_access WHERE organization_id = $1",
            &[&Uuid::parse_str(&second.id).expect("organization id")],
        )
        .await
        .expect("count grants")
        .get(0);
    assert_eq!(rows, 0, "deleting an organization removes its grants");

    // A listed model is unrestricted: it is not in the document, even
    // though a grant on it exists.
    grant(server, model, &first.id).await;
    set_listed(server, model, true).await;
    assert_eq!(internal_entry(server, model).await, None);
}

/// (d) Authentication of the internal document: the service token only.
#[tokio::test]
async fn internal_document_requires_the_service_token() {
    let fixture = setup().await;
    let server = &fixture.server;

    let missing = server.get("/v1/internal/model-access").await;
    assert_eq!(missing.status_code(), 401, "{}", missing.text());

    let org = organization(server).await;
    for token in ["not-the-token", &org.api_key, &get_session_id()] {
        let response = server
            .get("/v1/internal/model-access")
            .add_header("Authorization", format!("Bearer {token}"))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .await;
        assert_eq!(response.status_code(), 401, "{}", response.text());
    }

    // Disabled until the token is configured.
    let unconfigured = setup_test_server().await;
    let response = unconfigured
        .get("/v1/internal/model-access")
        .add_header("Authorization", format!("Bearer {INTERNAL_TOKEN}"))
        .await;
    assert_eq!(response.status_code(), 503, "{}", response.text());
    assert_eq!(
        response.json::<Value>()["error"]["type"],
        "endpoint_disabled"
    );
}

/// (e) The ITA token route is public, so no organization is known: an
/// unlisted model is unknown there, whoever holds a grant.
#[tokio::test]
async fn public_ita_token_route_treats_an_unlisted_model_as_unknown() {
    let fake_ita = FakeIta::start(FakeItaMode::Success).await;
    let server = setup_ita_server(&fake_ita, ItaServerMode::Enabled { max_retries: 0 }).await;
    let suffix = Uuid::new_v4();
    let model = format!("access-ita/model-{suffix}");
    let alias = format!("access-ita-alias-{suffix}");
    upsert_models(
        &server,
        vec![(&model, model_fields(json!({ "aliases": [alias] })))],
    )
    .await;
    // A grant changes nothing for an anonymous caller.
    let org = organization(&server).await;
    grant(&server, &model, &org.id).await;

    let token = |model: &str, strict: bool| {
        let mut request = server.get(&format!(
            "/v1/attestation/ita-token?model={}&nonce={NONCE}",
            urlencoding::encode(model)
        ));
        if strict {
            request = request.add_header("x-no-aliasing", "true");
        }
        request
    };
    for (name, strict) in [(&model, false), (&alias, false), (&alias, true)] {
        let unknown = unknown_twin(name);
        let refused = token(name, strict).await;
        assert_eq!(refused.status_code(), 400, "{}", refused.text());
        assert_eq!(
            refused.json::<Value>()["error"],
            json!({
                "message": format!("Model '{name}' not found. It's not a valid model name or alias."),
                "type": "invalid_request_error",
                "param": "model",
                "code": null,
            })
        );
        assert!(!refused.text().contains(&format!("'{model}'")) || name == &model);
        assert_eq!(
            observe(&refused, name),
            observe(&token(&unknown, strict).await, &unknown)
        );
    }
    // No model evidence was appraised: only verifier nonces were fetched.
    assert!(
        fake_ita
            .paths()
            .iter()
            .all(|path| *path == "/appraisal/v2/nonce"),
        "{:?}",
        fake_ita.paths()
    );
}

/// The attestation report takes an optional API key. Without one no
/// organization is known, so an unlisted model is unknown even when some
/// organization holds a grant; that organization's key still gets the report.
#[tokio::test]
async fn anonymous_attestation_report_treats_an_unlisted_model_as_unknown() {
    let fixture = setup().await;
    let server = &fixture.server;
    let org = organization(server).await;
    grant(server, &fixture.text, &org.id).await;

    let report = |model: &str, api_key: Option<&str>, strict: bool| {
        let mut request = server.get(&format!(
            "/v1/attestation/report?model={}&nonce={NONCE}",
            urlencoding::encode(model)
        ));
        if let Some(api_key) = api_key {
            request = request.add_header("Authorization", format!("Bearer {api_key}"));
        }
        if strict {
            request = request.add_header("x-no-aliasing", "true");
        }
        request
    };
    for (name, strict) in [
        (&fixture.text, false),
        (&fixture.text_alias, false),
        (&fixture.text_alias, true),
    ] {
        let unknown = unknown_twin(name);
        let refused = report(name, None, strict).await;
        assert_eq!(refused.status_code(), 400, "{}", refused.text());
        assert!(
            !refused.text().contains(&format!("'{}'", fixture.text)) || name == &fixture.text,
            "the anonymous report named the canonical model: {}",
            refused.text()
        );
        assert_eq!(
            observe(&refused, name),
            observe(&report(&unknown, None, strict).await, &unknown)
        );
    }

    let granted = report(&fixture.text, Some(&org.api_key), false).await;
    assert_eq!(granted.status_code(), 200, "{}", granted.text());
}

/// The public catalog detail route keeps answering 404 for an unlisted model.
#[tokio::test]
async fn public_model_detail_does_not_show_an_unlisted_model() {
    let fixture = setup().await;
    let server = &fixture.server;
    for name in [&fixture.text, &fixture.text_alias] {
        let unknown = unknown_twin(name);
        let detail = |model: &str| server.get(&format!("/v1/model/{}", urlencoding::encode(model)));
        assert_eq!(
            observe(&detail(name).await, name),
            observe(&detail(&unknown).await, &unknown)
        );
    }
}

/// (f) Usage that was served is billed: `POST /v1/internal/usage` records
/// usage on an unlisted model for an organization that holds no grant.
#[tokio::test]
async fn usage_is_recorded_for_an_organization_without_a_grant() {
    let fixture = setup().await;
    let server = &fixture.server;
    let org = setup_org_with_credits(server, 10_000_000_000i64).await;
    let workspace_id = list_workspaces(server, org.id.clone())
        .await
        .first()
        .expect("org has a default workspace")
        .id
        .clone();
    let key = create_api_key_in_workspace(server, workspace_id.clone(), "usage".to_string()).await;

    // The organization holds no grant: it cannot call the model here...
    let access: Value = admin_call(
        server,
        Method::GET,
        &format!("/v1/admin/organizations/{}/model-access", org.id),
        &get_session_id(),
    )
    .await
    .json();
    assert_eq!(access["models"], json!([]));
    let refused = send(
        server,
        Route::Embeddings,
        key.key.as_deref().expect("a new key carries its secret"),
        &fixture.text,
    )
    .await;
    assert_eq!(refused.status_code(), 404);

    // ...yet usage a trusted service reports for it is recorded and priced.
    let response = server
        .post("/v1/internal/usage")
        .add_header("Authorization", format!("Bearer {INTERNAL_TOKEN}"))
        .json(&json!({
            "organization_id": org.id,
            "workspace_id": workspace_id,
            "api_key_id": key.id,
            "type": "chat_completion",
            "model": fixture.text,
            "input_tokens": 1000,
            "output_tokens": 200,
            "id": format!("model-access-usage-{}", Uuid::new_v4())
        }))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body: Value = response.json();
    assert_eq!(body["model"], fixture.text.as_str());
    assert_eq!(body["input_cost"], 1_000_000_000i64);
    assert_eq!(body["output_cost"], 400_000_000i64);
}

fn settings_path(organization_id: &str) -> String {
    format!("/v1/organizations/{organization_id}/settings")
}

/// `PATCH /v1/organizations/{org_id}/settings` with `access_token`.
async fn patch_settings(
    server: &axum_test::TestServer,
    access_token: &str,
    organization_id: &str,
    body: Value,
) -> axum_test::TestResponse {
    server
        .patch(&settings_path(organization_id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&body)
        .await
}

/// The owner's access token (the mock session user owns every test
/// organization).
async fn owner_token(server: &axum_test::TestServer) -> String {
    get_access_token_from_refresh_token(server, get_session_id()).await
}

/// Limit `organization` to `models` as its owner; returns what was stored.
async fn allow_models(
    server: &axum_test::TestServer,
    organization_id: &str,
    models: &[&str],
) -> Vec<String> {
    let response = patch_settings(
        server,
        &owner_token(server).await,
        organization_id,
        json!({ "allowed_models": models }),
    )
    .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    response
        .json::<api::models::OrganizationSettingsResponse>()
        .settings
        .allowed_models
        .expect("a filter is set")
}

/// Allow every model again, as the owner.
async fn allow_every_model(server: &axum_test::TestServer, organization_id: &str) {
    let response = patch_settings(
        server,
        &owner_token(server).await,
        organization_id,
        json!({ "allowed_models": null }),
    )
    .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(
        response
            .json::<api::models::OrganizationSettingsResponse>()
            .settings
            .allowed_models,
        None
    );
}

/// An organization that set `allowed_models` gets, for any model it did not
/// list, exactly the answer an unknown model gets, on every route that takes
/// a model name, by name and by alias. Listing the model opens it; other
/// organizations keep every listed model.
#[tokio::test]
async fn an_organization_filter_closes_unlisted_models_on_every_route() {
    let fixture = setup().await;
    let server = &fixture.server;
    let kinds = [
        ModelKind::Text,
        ModelKind::Decisions,
        ModelKind::Anthropic,
        ModelKind::NativeResponses,
    ];
    for kind in kinds {
        set_listed(server, fixture.model(kind), true).await;
    }
    let filtered = organization(server).await;
    let other = organization(server).await;
    allow_models(server, &filtered.id, &[&fixture.successor]).await;

    for &route in ROUTES {
        let model = fixture.model(route.model_kind());
        let unknown = unknown_twin(model);
        let missing = observe(
            &send(server, route, &filtered.api_key, &unknown).await,
            &unknown,
        );
        let refused = observe(&send(server, route, &filtered.api_key, model).await, model);
        assert!(
            (400..500).contains(&missing.status) || missing.body.contains("event: response.failed"),
            "{route:?}: an unknown model must be an error: {missing:?}"
        );
        assert_eq!(
            refused, missing,
            "{route:?}: a model the organization did not allow must look unknown"
        );
        if route.mock_serves_it() {
            let served = send(server, route, &other.api_key, model).await;
            assert_eq!(
                served.status_code(),
                200,
                "{route:?}: an organization without a filter keeps every listed model: {}",
                served.text()
            );
        }
        if route.model_kind() == ModelKind::Text {
            let alias = &fixture.text_alias;
            let unknown_alias = unknown_twin(alias);
            let missing = observe(
                &send(server, route, &filtered.api_key, &unknown_alias).await,
                &unknown_alias,
            );
            let refused = observe(&send(server, route, &filtered.api_key, alias).await, alias);
            assert_eq!(refused, missing, "{route:?}: by alias");
        }
    }

    // Allowing the models opens them, at once, on every route.
    let all: Vec<&str> = kinds.iter().map(|&kind| fixture.model(kind)).collect();
    allow_models(server, &filtered.id, &all).await;
    for &route in ROUTES {
        let model = fixture.model(route.model_kind());
        let unknown = unknown_twin(model);
        let missing = observe(
            &send(server, route, &filtered.api_key, &unknown).await,
            &unknown,
        );
        let allowed = send(server, route, &filtered.api_key, model).await;
        if route.mock_serves_it() {
            assert_eq!(
                allowed.status_code(),
                200,
                "{route:?}: an allowed model must be served: {}",
                allowed.text()
            );
        }
        assert_ne!(
            observe(&allowed, model),
            missing,
            "{route:?}: an allowed model must move past the unknown-model answer"
        );
    }

    // The successor is no longer on the list: closed. Null opens it again.
    fixture
        .pool
        .register_provider(fixture.successor.clone(), fixture.provider.clone())
        .await;
    let successor = &fixture.successor;
    let unknown = unknown_twin(successor);
    let key = &filtered.api_key;
    assert_eq!(
        observe(
            &send(server, Route::ChatCompletions, key, successor).await,
            successor
        ),
        observe(
            &send(server, Route::ChatCompletions, key, &unknown).await,
            &unknown
        )
    );
    allow_every_model(server, &filtered.id).await;
    let served = send(server, Route::ChatCompletions, key, successor).await;
    assert_eq!(served.status_code(), 200, "{}", served.text());
}

/// The filter only narrows: an unlisted model on the list still needs a
/// grant, and naming one without a grant is refused like an unknown model,
/// so the setting discloses nothing. Names are stored canonical.
#[tokio::test]
async fn allowed_models_are_validated_and_stored_canonical() {
    let fixture = setup().await;
    let server = &fixture.server;
    set_listed(server, &fixture.text, true).await;
    let org = organization(server).await;
    let token = owner_token(server).await;
    let unknown = unknown_twin(&fixture.decisions);

    for (body, expected) in [
        (
            json!({ "allowed_models": [] }),
            "send null to allow every model",
        ),
        (json!({ "allowed_models": [unknown] }), "not found"),
        // Unlisted, no grant: the same answer as an unknown name.
        (
            json!({ "allowed_models": [fixture.decisions] }),
            "not found",
        ),
    ] {
        let response = patch_settings(server, &token, &org.id, body.clone()).await;
        assert_eq!(response.status_code(), 400, "{body}: {}", response.text());
        assert!(
            response.text().contains(expected),
            "{body}: {}",
            response.text()
        );
    }
    let refused_unlisted = patch_settings(
        server,
        &token,
        &org.id,
        json!({ "allowed_models": [fixture.decisions] }),
    )
    .await
    .text()
    .replace(&fixture.decisions, "<model>");
    let refused_unknown = patch_settings(
        server,
        &token,
        &org.id,
        json!({ "allowed_models": [unknown] }),
    )
    .await
    .text()
    .replace(&unknown, "<model>");
    assert_eq!(refused_unlisted, refused_unknown);

    // An alias is stored as the canonical name, duplicates once.
    let alias = fixture.text_alias.as_str();
    assert_eq!(
        allow_models(server, &org.id, &[alias, &fixture.text]).await,
        vec![fixture.text.clone()]
    );
    let settings = server
        .get(&settings_path(&org.id))
        .add_header("Authorization", format!("Bearer {token}"))
        .await
        .json::<api::models::OrganizationSettingsResponse>();
    assert_eq!(
        settings.settings.allowed_models,
        Some(vec![fixture.text.clone()])
    );

    // With a grant the unlisted model can be listed, and is served.
    grant(server, &fixture.decisions, &org.id).await;
    let mut both = vec![fixture.text.clone(), fixture.decisions.clone()];
    both.sort();
    assert_eq!(
        allow_models(server, &org.id, &[&fixture.decisions, &fixture.text]).await,
        both
    );
    let served = send(server, Route::SystemOne, &org.api_key, &fixture.decisions).await;
    assert_eq!(served.status_code(), 200, "{}", served.text());
}

/// Owners and admins manage the filter; members cannot. (The settings routes
/// take a signed-in session, never an API key; the e2e mock auth accepts any
/// token as a session, so that half is not observable here.)
#[tokio::test]
async fn owners_and_admins_manage_allowed_models() {
    let fixture = setup().await;
    let server = &fixture.server;
    set_listed(server, &fixture.text, true).await;
    let org = organization(server).await;
    let (admin_session, _) = setup_unique_test_session(&fixture.database).await;
    let (member_session, _) = setup_unique_test_session(&fixture.database).await;
    let admin_id = Uuid::parse_str(admin_session.trim_start_matches("rt_")).unwrap();
    let member_id = Uuid::parse_str(member_session.trim_start_matches("rt_")).unwrap();
    let org_id = Uuid::parse_str(&org.id).unwrap();
    fixture
        .database
        .pool()
        .get()
        .await
        .expect("database connection")
        .execute(
            "INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'admin'), ($1, $3, 'member')",
            &[&org_id, &admin_id, &member_id],
        )
        .await
        .expect("insert organization roles");
    let body = json!({ "allowed_models": [fixture.text] });

    let member = patch_settings(server, &member_session, &org.id, body.clone()).await;
    assert_eq!(member.status_code(), 403, "{}", member.text());
    // The same refusal whatever the names: a member learns nothing from
    // validation either.
    for names in [
        json!([]),
        json!([unknown_twin(&fixture.text)]),
        json!([fixture.decisions]),
    ] {
        let refused = patch_settings(
            server,
            &member_session,
            &org.id,
            json!({ "allowed_models": names }),
        )
        .await;
        assert_eq!(
            (refused.status_code(), refused.text()),
            (member.status_code(), member.text()),
            "{names}"
        );
    }

    // A non-member gets the same refusal whatever the names, so it cannot
    // learn which unlisted models the organization holds a grant for.
    grant(server, &fixture.decisions, &org.id).await;
    let (outsider_session, _) = setup_unique_test_session(&fixture.database).await;
    for names in [
        json!([fixture.decisions]),
        json!([unknown_twin(&fixture.decisions)]),
    ] {
        let outsider = patch_settings(
            server,
            &outsider_session,
            &org.id,
            json!({ "allowed_models": names }),
        )
        .await;
        assert_eq!(outsider.status_code(), 403, "{names}: {}", outsider.text());
    }
    // Nothing was stored.
    let settings = server
        .get(&settings_path(&org.id))
        .add_header(
            "Authorization",
            format!("Bearer {}", owner_token(server).await),
        )
        .await
        .json::<api::models::OrganizationSettingsResponse>();
    assert_eq!(settings.settings.allowed_models, None);

    let admin = patch_settings(server, &admin_session, &org.id, body).await;
    assert_eq!(admin.status_code(), 200, "{}", admin.text());
}

async fn catalog(
    server: &axum_test::TestServer,
    api_key: Option<&str>,
) -> (Vec<String>, Option<String>) {
    let mut request = server.get("/v1/models");
    if let Some(api_key) = api_key {
        request = request.add_header("Authorization", format!("Bearer {api_key}"));
    }
    let response = request.await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(
        response.headers().get("vary").and_then(|v| v.to_str().ok()),
        Some("Authorization")
    );
    let cache_control = response
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let mut ids: Vec<String> = response
        .json::<api::models::ModelsResponse>()
        .data
        .into_iter()
        .map(|model| model.id)
        .collect();
    ids.sort();
    (ids, cache_control)
}

/// `GET /v1/models` answers the API key of an organization with a filter
/// with the models it may use. Everyone else gets the public catalog,
/// including a request whose key does not authenticate.
#[tokio::test]
async fn models_catalog_lists_only_the_allowed_models_of_a_filtered_organization() {
    let fixture = setup().await;
    let server = &fixture.server;
    set_listed(server, &fixture.text, true).await;
    let filtered = organization(server).await;
    let other = organization(server).await;

    let (public, _) = catalog(server, None).await;
    assert!(public.contains(&fixture.text));
    assert!(public.contains(&fixture.successor));
    assert!(!public.contains(&fixture.decisions), "unlisted stays out");

    grant(server, &fixture.decisions, &filtered.id).await;
    allow_models(server, &filtered.id, &[&fixture.text, &fixture.decisions]).await;

    let (own, own_cache) = catalog(server, Some(&filtered.api_key)).await;
    let mut expected = vec![fixture.text.clone(), fixture.decisions.clone()];
    expected.sort();
    assert_eq!(own, expected);
    // The same with the key as `x-api-key`, as an Anthropic SDK sends it.
    let by_x_api_key = server
        .get("/v1/models")
        .add_header("x-api-key", filtered.api_key.clone())
        .await;
    let mut ids: Vec<String> = by_x_api_key
        .json::<api::models::ModelsResponse>()
        .data
        .into_iter()
        .map(|model| model.id)
        .collect();
    ids.sort();
    assert_eq!(ids, expected);
    assert!(
        own_cache.as_deref().is_some_and(|v| v.contains("no-store")),
        "{own_cache:?}"
    );

    for api_key in [
        None,
        Some(other.api_key.as_str()),
        Some("sk-not-a-real-key"),
    ] {
        let (ids, _) = catalog(server, api_key).await;
        assert_eq!(ids, public, "{api_key:?}");
    }

    allow_every_model(server, &filtered.id).await;
    let (ids, _) = catalog(server, Some(&filtered.api_key)).await;
    assert_eq!(ids, public);
}

/// A fixture whose router is also served on a real port, with the OHTTP
/// gateway on. The gateway sends the request it decrypts to
/// `127.0.0.1:{config.server.port}`, so the port must be bound first.
async fn setup_with_ohttp() -> Fixture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let port = listener.local_addr().expect("bound address").port();
    let fixture = setup_with(move |config| {
        config.server.ohttp_enabled = true;
        config.server.port = port;
    })
    .await;
    let router = fixture.router.clone();
    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("loopback server");
    });
    fixture
}

/// The inner answer of an OHTTP request: (status, body).
struct OhttpAnswer {
    status: u16,
    body: String,
}

/// Encrypt `POST {path}` with `body` (and the inner `headers`) to the
/// gateway's key, post it to `/ohttp` with `Authorization: Bearer
/// {outer_api_key}` as a relay does, and decrypt the answer.
async fn ohttp_post(
    server: &axum_test::TestServer,
    outer_api_key: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &Value,
) -> OhttpAnswer {
    let key_config = server.get("/.well-known/ohttp-gateway").await;
    assert_eq!(key_config.status_code(), 200, "{}", key_config.text());
    let mut key_config =
        ohttp::KeyConfig::decode(key_config.as_bytes()).expect("a valid key configuration");
    let client = ohttp::ClientRequest::from_config(&mut key_config).expect("an OHTTP client");

    let mut inner = bhttp::Message::request(
        b"POST".to_vec(),
        b"https".to_vec(),
        b"api.invalid".to_vec(),
        path.as_bytes().to_vec(),
    );
    inner.put_header("content-type", "application/json");
    for (name, value) in headers {
        inner.put_header(*name, *value);
    }
    inner.write_content(serde_json::to_vec(body).expect("a JSON body"));
    let mut encoded = Vec::new();
    inner
        .write_bhttp(bhttp::Mode::KnownLength, &mut encoded)
        .expect("a BHTTP request");
    let (encapsulated, client_response) = client.encapsulate(&encoded).expect("encapsulated");

    let response = server
        .post("/ohttp")
        .add_header("Authorization", format!("Bearer {outer_api_key}"))
        .add_header("Content-Type", "message/ohttp-req")
        .bytes(Bytes::from(encapsulated))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let decrypted = client_response
        .decapsulate(response.as_bytes())
        .expect("a response for this request");
    let inner =
        bhttp::Message::read_bhttp(&mut std::io::Cursor::new(decrypted)).expect("a BHTTP response");
    OhttpAnswer {
        status: inner.control().status().expect("a response status").code(),
        body: String::from_utf8_lossy(inner.content()).into_owned(),
    }
}

/// A relay forwards an encrypted request to `POST /ohttp` with the caller's
/// key as the outer `Authorization`, and cannot see which model or route the
/// request names. The gateway sends the decrypted request through the same
/// routes over a loopback connection, so the organization's filter holds on
/// that path too, whatever the encrypted request says, including its own
/// `Authorization`.
#[tokio::test]
async fn an_organization_filter_holds_for_requests_relayed_through_ohttp() {
    let fixture = setup_with_ohttp().await;
    let server = &fixture.server;
    let allowed = &fixture.text;
    let refused = &fixture.successor;
    set_listed(server, allowed, true).await;
    fixture
        .pool
        .register_provider(refused.clone(), fixture.provider.clone())
        .await;
    let filtered = organization(server).await;
    let other = organization(server).await;
    allow_models(server, &filtered.id, &[allowed]).await;

    let chat = |model: &str| {
        json!({
            "model": model,
            "messages": [{ "role": "user", "content": "Hello" }],
            "max_tokens": 16
        })
    };
    let embedding = |model: &str| json!({ "model": model, "input": "Hello" });
    let unknown = unknown_twin(refused);

    for (path, body, unknown_body) in [
        ("/v1/chat/completions", chat(refused), chat(&unknown)),
        ("/v1/embeddings", embedding(refused), embedding(&unknown)),
    ] {
        let missing = ohttp_post(server, &filtered.api_key, path, &[], &unknown_body).await;
        assert!(
            (400..500).contains(&missing.status),
            "{path}: {}",
            missing.body
        );
        let blank = |answer: &OhttpAnswer, model: &str| {
            (answer.status, answer.body.replace(model, "<model>"))
        };
        let expected = blank(&missing, &unknown);

        let answer = ohttp_post(server, &filtered.api_key, path, &[], &body).await;
        assert_eq!(
            blank(&answer, refused),
            expected,
            "{path}: a model the organization did not allow must look unknown through OHTTP"
        );

        // The outer key wins over an inner one of an organization without a
        // filter, so the encrypted request cannot switch keys.
        let inner_key = format!("Bearer {}", other.api_key);
        let answer = ohttp_post(
            server,
            &filtered.api_key,
            path,
            &[("authorization", inner_key.as_str())],
            &body,
        )
        .await;
        assert_eq!(blank(&answer, refused), expected, "{path}: inner key");
    }

    // The allowed model is served through OHTTP.
    let served = ohttp_post(
        server,
        &filtered.api_key,
        "/v1/chat/completions",
        &[],
        &chat(allowed),
    )
    .await;
    assert_eq!(served.status, 200, "{}", served.body);

    // The refusal above came from the filter: without it, the same request
    // is served.
    allow_every_model(server, &filtered.id).await;
    let served = ohttp_post(
        server,
        &filtered.api_key,
        "/v1/chat/completions",
        &[],
        &chat(refused),
    )
    .await;
    assert_eq!(served.status, 200, "{}", served.body);
}
