//! Middleware tests for MPP: the auth wrapper and the settle layer, wired the way
//! `build_completion_routes` wires them (body hash outermost, settle innermost, nested under
//! `/v1`), against an in-memory claim store and a scripted Tempo node.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, Method, StatusCode},
    middleware::{from_fn_with_state, Next},
    response::{IntoResponse, Response},
    routing::post,
    Extension, Router,
};
use chrono::{DateTime, Utc};
use config::{MppModelPrice, MppSettings, MppTempoSettings};
use serde_json::{json, Value};
use services::common::RepositoryError;
use services::mpp::testing::{
    bound_payment_receipt, credential_header, hash_payload, payment_receipt, FakeTempo, SendMode,
};
use services::mpp::{
    protocol, ClaimState, InMemoryClaimRepository, MppService, RpcError, CHAT_COMPLETIONS_ROUTE,
};
use services::organization::{Organization, OrganizationId};
use services::reporting_tokens::{
    CreateOrganizationReportingTokenRequest, CreatedOrganizationReportingToken,
    OrganizationReportingToken, OrganizationReportingTokenService,
    ValidatedOrganizationReportingToken,
};
use services::workspace::{
    ApiKey, ApiKeyId, ApiKeyOrderBy, ApiKeyOrderDirection, ApiKeyRepository, CreateApiKeyRequest,
    Workspace, WorkspaceId, WorkspaceOrderBy, WorkspaceOrderDirection, WorkspaceRepository,
};
use tower::ServiceExt;
use uuid::Uuid;

use super::*;
use crate::middleware::auth::{AuthState, AuthenticatedApiKey};
use crate::middleware::body_hash::{body_hash_middleware, BodyHashLimit};

const REALM: &str = "cloud-api.near.ai";
const SECRET: &str = "0123456789abcdef0123456789abcdef-hmac";
const SERVICE_KEY: &str = "sk-live-service-key-0001";
const BEARER_KEY: &str = "sk-live-customer-key-0002";
const MODEL: &str = "zai-org/GLM-5";
const CURRENCY: [u8; 20] = [0x20; 20];
const RECIPIENT: [u8; 20] = [0xaa; 20];
const PAYER: [u8; 20] = [0x40; 20];
const AMOUNT: u128 = 20_000;
const TX: [u8; 32] = [0x11; 32];

// ----- fakes for the authentication state -----

struct Keys;

fn api_key(key: &str) -> ApiKey {
    ApiKey {
        id: ApiKeyId(Uuid::from_u128(5).to_string()),
        key: None,
        key_prefix: key.chars().take(12).collect(),
        name: "k".to_string(),
        workspace_id: WorkspaceId(Uuid::from_u128(8)),
        created_by_user_id: services::auth::UserId(Uuid::nil()),
        created_at: Utc::now(),
        expires_at: None,
        last_used_at: None,
        is_active: true,
        deleted_at: None,
        spend_limit: None,
        usage: None,
    }
}

#[async_trait]
impl ApiKeyRepository for Keys {
    async fn validate(&self, key: String) -> Result<Option<ApiKey>, RepositoryError> {
        Ok((key == SERVICE_KEY || key == BEARER_KEY).then(|| api_key(&key)))
    }
    async fn create(&self, _: CreateApiKeyRequest) -> Result<ApiKey, RepositoryError> {
        unimplemented!()
    }
    async fn get_by_id(&self, _: ApiKeyId) -> Result<Option<ApiKey>, RepositoryError> {
        unimplemented!()
    }
    async fn list_by_workspace_paginated(
        &self,
        _: WorkspaceId,
        _: i64,
        _: i64,
        _: Option<ApiKeyOrderBy>,
        _: Option<ApiKeyOrderDirection>,
    ) -> Result<Vec<ApiKey>, RepositoryError> {
        unimplemented!()
    }
    async fn delete(&self, _: ApiKeyId) -> Result<bool, RepositoryError> {
        unimplemented!()
    }
    async fn update_last_used(&self, _: ApiKeyId) -> Result<(), RepositoryError> {
        Ok(())
    }
    async fn update_spend_limit(
        &self,
        _: ApiKeyId,
        _: Option<i64>,
    ) -> Result<ApiKey, RepositoryError> {
        unimplemented!()
    }
    async fn update(
        &self,
        _: ApiKeyId,
        _: Option<String>,
        _: Option<Option<DateTime<Utc>>>,
        _: Option<Option<i64>>,
        _: Option<bool>,
    ) -> Result<ApiKey, RepositoryError> {
        unimplemented!()
    }
    async fn count_by_workspace(&self, _: WorkspaceId) -> Result<i64, RepositoryError> {
        unimplemented!()
    }
    async fn check_name_duplication(
        &self,
        _: WorkspaceId,
        _: &str,
    ) -> Result<bool, RepositoryError> {
        unimplemented!()
    }
    async fn revoke(&self, _: ApiKeyId) -> Result<bool, RepositoryError> {
        unimplemented!()
    }
    async fn get_all_active_key_hashes(&self) -> Result<Vec<String>, RepositoryError> {
        Ok(vec![])
    }
    async fn get_active_key_hashes_created_after(
        &self,
        _: DateTime<Utc>,
    ) -> Result<Vec<String>, RepositoryError> {
        Ok(vec![])
    }
}

struct Workspaces;

#[async_trait]
impl WorkspaceRepository for Workspaces {
    async fn get_by_id(&self, _: WorkspaceId) -> Result<Option<Workspace>, RepositoryError> {
        unimplemented!()
    }
    async fn get_by_name(&self, _: Uuid, _: &str) -> Result<Option<Workspace>, RepositoryError> {
        unimplemented!()
    }
    async fn get_workspace_with_organization(
        &self,
        id: WorkspaceId,
    ) -> Result<Option<(Workspace, Organization)>, RepositoryError> {
        let owner = services::auth::UserId(Uuid::nil());
        Ok(Some((
            Workspace {
                id,
                name: "w".to_string(),
                description: None,
                organization_id: OrganizationId(Uuid::from_u128(9)),
                created_by_user_id: owner.clone(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
                is_active: true,
                settings: None,
            },
            Organization {
                request_priority: 0,
                id: OrganizationId(Uuid::from_u128(9)),
                name: "o".to_string(),
                description: None,
                owner_id: owner,
                settings: json!({}),
                is_active: true,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
        )))
    }
    async fn list_by_organization(
        &self,
        _: OrganizationId,
    ) -> Result<Vec<Workspace>, RepositoryError> {
        unimplemented!()
    }
    async fn list_by_organization_paginated(
        &self,
        _: OrganizationId,
        _: i64,
        _: i64,
        _: Option<WorkspaceOrderBy>,
        _: Option<WorkspaceOrderDirection>,
    ) -> Result<Vec<Workspace>, RepositoryError> {
        unimplemented!()
    }
    async fn create(
        &self,
        _: String,
        _: Option<String>,
        _: OrganizationId,
        _: services::auth::UserId,
    ) -> Result<Workspace, RepositoryError> {
        unimplemented!()
    }
    async fn update(
        &self,
        _: WorkspaceId,
        _: Option<String>,
        _: Option<String>,
        _: Option<Value>,
    ) -> Result<Option<Workspace>, RepositoryError> {
        unimplemented!()
    }
    async fn delete(&self, _: WorkspaceId) -> Result<bool, RepositoryError> {
        unimplemented!()
    }
    async fn count_by_organization(&self, _: OrganizationId) -> Result<i64, RepositoryError> {
        unimplemented!()
    }
    async fn list_by_user(
        &self,
        _: services::auth::UserId,
        _: i64,
    ) -> Result<Vec<Workspace>, RepositoryError> {
        unimplemented!()
    }
}

struct NoReportingTokens;

#[async_trait]
impl OrganizationReportingTokenService for NoReportingTokens {
    async fn create(
        &self,
        _: CreateOrganizationReportingTokenRequest,
    ) -> Result<CreatedOrganizationReportingToken, RepositoryError> {
        unimplemented!()
    }
    async fn validate(
        &self,
        _: &str,
    ) -> Result<Option<ValidatedOrganizationReportingToken>, RepositoryError> {
        Ok(None)
    }
    async fn get_by_id(
        &self,
        _: Uuid,
    ) -> Result<Option<OrganizationReportingToken>, RepositoryError> {
        unimplemented!()
    }
    async fn list_active_by_organization(
        &self,
        _: Uuid,
    ) -> Result<Vec<OrganizationReportingToken>, RepositoryError> {
        unimplemented!()
    }
    async fn revoke(&self, _: Uuid, _: Uuid) -> Result<bool, RepositoryError> {
        unimplemented!()
    }
}

fn auth_state() -> AuthState {
    AuthState::new(
        Arc::new(services::auth::OAuthManager::new(None, None).unwrap()),
        Arc::new(services::auth::MockAuthService {
            apikey_repository: Arc::new(Keys),
        }),
        Arc::new(Workspaces),
        Arc::new(NoReportingTokens),
        Arc::new(database::repositories::AdminAccessTokenRepository::new(
            database::DbPool::uninitialized(),
        )),
        Vec::new(),
        String::new(),
    )
}

// ----- the app under test -----

fn settings(service_key: &str) -> MppSettings {
    let mut prices = BTreeMap::new();
    prices.insert(
        MODEL.to_string(),
        MppModelPrice {
            amount: AMOUNT,
            max_tokens: 2048,
            max_request_bytes: 4096,
        },
    );
    MppSettings {
        realm: REALM.to_string(),
        secret_key: SECRET.to_string(),
        service_api_key: service_key.to_string(),
        challenge_ttl_secs: 600,
        tempo: MppTempoSettings {
            rpc_url: "https://rpc.test.invalid/secret-provider-key".to_string(),
            chain_id: 42431,
            currency: format!("0x{}", hex::encode(CURRENCY)),
            recipient: format!("0x{}", hex::encode(RECIPIENT)),
            rpc_timeout_secs: 5,
        },
        model_prices: prices,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Handler {
    Ok,
    Fail,
    Stream,
}

#[derive(Clone)]
struct HandlerState {
    calls: Arc<AtomicUsize>,
    mode: Arc<Mutex<Handler>>,
}

/// Records which key the request ran as and whether a payment was pending.
async fn handler(
    State(state): State<HandlerState>,
    Extension(key): Extension<AuthenticatedApiKey>,
    request: Request,
) -> Response {
    state.calls.fetch_add(1, Ordering::SeqCst);
    let pending = request.extensions().get::<PendingMppPayment>().is_some();
    let mode = *state.mode.lock().unwrap();
    match mode {
        Handler::Ok => {
            axum::Json(json!({"key_prefix": key.api_key.key_prefix, "pending": pending}))
                .into_response()
        }
        Handler::Fail => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        Handler::Stream => {
            let chunks = futures::stream::iter(vec![
                Ok::<_, std::convert::Infallible>(bytes::Bytes::from("data: one\n\n")),
                Ok(bytes::Bytes::from("data: [DONE]\n\n")),
            ]);
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(chunks))
                .unwrap()
        }
    }
}

/// Stands in for `usage_check_middleware`: 402 `no_credits` while blocked.
async fn usage_stub(
    State(blocked): State<Arc<AtomicBool>>,
    request: Request,
    next: Next,
) -> Response {
    if blocked.load(Ordering::SeqCst) {
        (
            StatusCode::PAYMENT_REQUIRED,
            axum::Json(ErrorResponse::new(
                "No credits available.".to_string(),
                "no_credits".to_string(),
            )),
        )
            .into_response()
    } else {
        next.run(request).await
    }
}

struct App {
    router: Router,
    repo: Arc<InMemoryClaimRepository>,
    chain: Arc<FakeTempo>,
    service: Option<Arc<MppService>>,
    calls: Arc<AtomicUsize>,
    mode: Arc<Mutex<Handler>>,
    blocked: Arc<AtomicBool>,
}

/// `enabled = false` is the production default: both middlewares hold `None`.
fn app_with(enabled: bool, service_key: &str) -> App {
    let repo = Arc::new(InMemoryClaimRepository::default());
    let chain = Arc::new(FakeTempo::default());
    let service = enabled.then(|| {
        Arc::new(
            MppService::new(&settings(service_key), repo.clone(), chain.clone())
                .unwrap()
                .with_recover_poll(2, Duration::ZERO),
        )
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let mode = Arc::new(Mutex::new(Handler::Ok));
    let blocked = Arc::new(AtomicBool::new(false));
    let state = HandlerState {
        calls: calls.clone(),
        mode: mode.clone(),
    };
    let group = Router::new()
        .route("/chat/completions", post(handler))
        .route("/embeddings", post(handler))
        .with_state(state)
        .layer(from_fn_with_state(
            MppSettleState {
                mpp: service.clone(),
            },
            mpp_settle_middleware,
        ))
        .layer(from_fn_with_state(blocked.clone(), usage_stub))
        .layer(from_fn_with_state(
            MppAuthState {
                auth: auth_state(),
                mpp: service.clone(),
            },
            mpp_auth_middleware,
        ))
        .layer(from_fn_with_state(
            BodyHashLimit(1 << 20),
            body_hash_middleware,
        ));
    App {
        router: Router::new().nest("/v1", group),
        repo,
        chain,
        service,
        calls,
        mode,
        blocked,
    }
}

fn app(enabled: bool) -> App {
    app_with(enabled, SERVICE_KEY)
}

/// The same chain without any MPP code: what "unchanged" means.
fn baseline_router() -> Router {
    let state = HandlerState {
        calls: Arc::new(AtomicUsize::new(0)),
        mode: Arc::new(Mutex::new(Handler::Ok)),
    };
    let group = Router::new()
        .route("/chat/completions", post(handler))
        .route("/embeddings", post(handler))
        .with_state(state)
        .layer(from_fn_with_state(
            auth_state(),
            crate::middleware::auth::auth_middleware_with_workspace_context,
        ))
        .layer(from_fn_with_state(
            BodyHashLimit(1 << 20),
            body_hash_middleware,
        ));
    Router::new().nest("/v1", group)
}

fn body() -> Vec<u8> {
    json!({"model": MODEL, "max_tokens": 100, "messages": [{"role": "user", "content": "hi"}]})
        .to_string()
        .into_bytes()
}

fn request(path: &str, body: &[u8], authorization: Option<&str>) -> Request {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(value) = authorization {
        builder = builder.header(header::AUTHORIZATION, value);
    }
    builder.body(Body::from(body.to_vec())).unwrap()
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

async fn send(router: &Router, request: Request) -> Reply {
    let response = router.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let body = axum::body::to_bytes(Body::new(body), 1 << 20)
        .await
        .unwrap();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: body.to_vec(),
    }
}

impl App {
    async fn post(&self, body: &[u8], authorization: Option<&str>) -> Reply {
        send(
            &self.router,
            request("/v1/chat/completions", body, authorization),
        )
        .await
    }

    /// Ask for a challenge, "pay" it on the fake chain, and return the credential header.
    async fn pay(&self, body: &[u8], tx: [u8; 32]) -> String {
        let service = self.service.as_ref().unwrap();
        let challenge = service
            .challenge_for_body(CHAT_COMPLETIONS_ROUTE, body)
            .unwrap();
        self.chain.mine(bound_payment_receipt(
            tx, PAYER, CURRENCY, RECIPIENT, AMOUNT, REALM, &challenge,
        ));
        credential_header(&challenge, hash_payload(tx), None)
    }
}

fn key(hash: [u8; 32]) -> String {
    format!("tempo:0x{}", hex::encode(hash))
}

fn no_secrets(reply: &Reply) {
    let text = reply.text();
    let headers = format!("{:?}", reply.headers);
    for secret in [SECRET, SERVICE_KEY, BEARER_KEY, "secret-provider-key"] {
        assert!(!text.contains(secret), "body leaks a secret");
        assert!(!headers.contains(secret), "headers leak a secret");
    }
}

// ----- disabled: nothing changes -----

#[tokio::test]
async fn disabled_mode_is_byte_for_byte_todays_behaviour() {
    let enabled_off = app(false);
    let baseline = baseline_router();
    let chain = FakeTempo::default();
    let _ = chain;
    let credential = "Payment e30";
    let probes: Vec<(&str, Vec<u8>, Option<&str>)> = vec![
        ("/v1/chat/completions", body(), None),
        ("/v1/chat/completions", body(), Some("Bearer sk-wrong")),
        ("/v1/chat/completions", body(), Some(credential)),
        (
            "/v1/chat/completions",
            body(),
            Some("Bearer sk-live-customer-key-0002"),
        ),
        ("/v1/chat/completions", b"not json".to_vec(), None),
        ("/v1/embeddings", body(), None),
        ("/v1/embeddings", body(), Some(credential)),
    ];
    for (path, payload, authorization) in probes {
        let ours = send(&enabled_off.router, request(path, &payload, authorization)).await;
        let theirs = send(&baseline, request(path, &payload, authorization)).await;
        assert_eq!(ours.status, theirs.status, "{path} {authorization:?}");
        assert_eq!(ours.body, theirs.body, "{path} {authorization:?}");
        assert_eq!(
            ours.header("content-type"),
            theirs.header("content-type"),
            "{path} {authorization:?}"
        );
        assert!(ours.header("www-authenticate").is_none());
        assert!(ours.header("payment-receipt").is_none());
    }
    // Spot-check the baseline itself: no Authorization is today's 401, not a 402.
    let reply = enabled_off.post(&body(), None).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.json()["error"]["type"], "missing_auth_header");
    let reply = enabled_off.post(&body(), Some("Payment e30")).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.json()["error"]["type"], "invalid_auth_header");
}

// ----- 402 challenge -----

#[tokio::test]
async fn no_authorization_on_a_priced_model_gets_a_402_challenge() {
    let app = app(true);
    let reply = app.post(&body(), None).await;
    assert_eq!(reply.status, StatusCode::PAYMENT_REQUIRED);
    let challenge = reply.header("www-authenticate").unwrap();
    assert!(challenge.starts_with("Payment id=\""), "{challenge}");
    assert!(challenge.contains("realm=\"cloud-api.near.ai\""));
    assert!(challenge.contains("method=\"tempo\", intent=\"charge\""));
    assert!(challenge.contains("expires=\""));
    assert!(challenge.contains("digest=\"sha-256=:"));
    assert!(challenge.contains("opaque=\""));
    assert_eq!(reply.header("cache-control"), Some("no-store"));
    assert_eq!(
        reply.header("content-type"),
        Some("application/problem+json")
    );
    let problem = reply.json();
    assert_eq!(
        problem["type"],
        "https://paymentauth.org/problems/payment-required"
    );
    assert_eq!(problem["status"], 402);
    assert_eq!(
        app.calls.load(Ordering::SeqCst),
        0,
        "the handler must not run unpaid"
    );
    no_secrets(&reply);

    // The advertised request is the configured price on the configured chain.
    let request_param = challenge
        .split("request=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let decoded = String::from_utf8(protocol::b64url_decode(request_param).unwrap()).unwrap();
    assert_eq!(
        decoded,
        format!(
            r#"{{"amount":"20000","currency":"0x{}","methodDetails":{{"chainId":42431}},"recipient":"0x{}"}}"#,
            hex::encode(CURRENCY),
            hex::encode(RECIPIENT)
        )
    );
}

#[tokio::test]
async fn every_402_carries_a_fresh_challenge_for_the_current_body() {
    let app = app(true);
    let first = app.post(&body(), None).await;
    let mut other = body();
    other.extend_from_slice(b"\n");
    let second = app.post(&other, None).await;
    assert_ne!(
        first.header("www-authenticate"),
        second.header("www-authenticate"),
        "the challenge is bound to the body"
    );
}

#[tokio::test]
async fn bearer_unpriced_unparseable_and_other_routes_are_unchanged_when_enabled() {
    let app = app(true);
    // Bearer path.
    let reply = app
        .post(&body(), Some(&format!("Bearer {BEARER_KEY}")))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["pending"], false);
    assert_eq!(reply.json()["key_prefix"], "sk-live-cust");
    assert!(reply.header("payment-receipt").is_none());
    assert_eq!(app.calls.load(Ordering::SeqCst), 1);
    // An unknown Bearer key is still 401.
    let reply = app.post(&body(), Some("Bearer sk-nope")).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.json()["error"]["type"], "invalid_api_key");

    // Unpriced model, unparseable body, no model: today's 401.
    for payload in [
        json!({"model": "other/model", "max_tokens": 5})
            .to_string()
            .into_bytes(),
        b"not json".to_vec(),
        json!({"max_tokens": 5}).to_string().into_bytes(),
    ] {
        let reply = app.post(&payload, None).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.json()["error"]["type"], "missing_auth_header");
        assert!(reply.header("www-authenticate").is_none());
    }
    // A Payment header on an unpriced model is not an MPP request either.
    let payload = json!({"model": "other/model", "max_tokens": 5})
        .to_string()
        .into_bytes();
    let reply = app.post(&payload, Some("Payment e30")).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    // Another route of the group never sees MPP, even with a priced body.
    let reply = send(&app.router, request("/v1/embeddings", &body(), None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert!(reply.header("www-authenticate").is_none());
    let reply = send(
        &app.router,
        request("/v1/embeddings", &body(), Some("Payment e30")),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.json()["error"]["type"], "invalid_auth_header");
    // Only POST is the inference route: a GET without a key stays today's 401, never a 402.
    let get = Request::builder()
        .method(Method::GET)
        .uri("/v1/chat/completions")
        .body(Body::from(body()))
        .unwrap();
    let reply = send(&app.router, get).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert!(reply.header("www-authenticate").is_none());
}

#[tokio::test]
async fn limit_violations_are_400_validation_errors_without_a_challenge() {
    let app = app(true);
    let cases = [
        (json!({"model": MODEL}), "max_tokens"),
        (json!({"model": MODEL, "max_tokens": 99999}), "2048"),
        (
            json!({"model": MODEL, "max_tokens": 10, "n": 3}),
            "n must be 1",
        ),
        (
            json!({"model": MODEL, "max_tokens": 10, "pad": "x".repeat(5000)}),
            "4096",
        ),
    ];
    for (payload, needle) in cases {
        let bytes = payload.to_string().into_bytes();
        for authorization in [None, Some("Payment e30")] {
            let reply = app.post(&bytes, authorization).await;
            assert_eq!(
                reply.status,
                StatusCode::BAD_REQUEST,
                "{needle} {authorization:?}"
            );
            assert_eq!(reply.json()["error"]["type"], "validation_error");
            let message = reply.json()["error"]["message"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(message.contains(needle), "{message}");
            assert!(reply.header("www-authenticate").is_none());
        }
    }
    // Request content is never echoed back.
    let secret_prompt = json!({"model": MODEL, "max_tokens": 99999, "messages": [{"role": "user", "content": "my-private-prompt"}]});
    let reply = app.post(secret_prompt.to_string().as_bytes(), None).await;
    assert!(!reply.text().contains("my-private-prompt"));
    assert_eq!(app.calls.load(Ordering::SeqCst), 0);
}

// ----- paid path -----

#[tokio::test]
async fn a_paid_request_runs_as_the_service_key_and_returns_a_receipt() {
    let app = app(true);
    let credential = app.pay(&body(), TX).await;
    let reply = app.post(&body(), Some(&credential)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        reply.json()["key_prefix"],
        "sk-live-serv",
        "runs as the service key"
    );
    assert_eq!(reply.json()["pending"], true);
    assert_eq!(app.calls.load(Ordering::SeqCst), 1);
    assert_eq!(app.repo.state_of(&key(TX)), Some(ClaimState::Served));
    assert_eq!(reply.header("cache-control"), Some("private"));
    let receipt = reply.header("payment-receipt").unwrap();
    let decoded: Value =
        serde_json::from_slice(&protocol::b64url_decode(receipt).unwrap()).unwrap();
    assert_eq!(decoded["method"], "tempo");
    assert_eq!(decoded["status"], "success");
    assert_eq!(decoded["reference"], format!("0x{}", hex::encode(TX)));
    assert!(DateTime::parse_from_rfc3339(decoded["timestamp"].as_str().unwrap()).is_ok());
    no_secrets(&reply);
}

#[tokio::test]
async fn a_paid_transaction_credential_works_end_to_end() {
    let app = app(true);
    let service = app.service.as_ref().unwrap();
    let challenge = service
        .challenge_for_body(CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    let (payload, raw) = services::mpp::testing::transaction_payload(3);
    let hash = services::mpp::tempo::transaction_hash(&raw);
    app.chain.mine(bound_payment_receipt(
        hash, PAYER, CURRENCY, RECIPIENT, AMOUNT, REALM, &challenge,
    ));
    let credential = credential_header(&challenge, payload, None);
    let reply = app.post(&body(), Some(&credential)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(app.chain.send_calls(), 1);
    assert_eq!(app.repo.state_of(&key(hash)), Some(ClaimState::Served));
}

#[tokio::test]
async fn a_replayed_credential_is_403_without_a_challenge() {
    let app = app(true);
    let credential = app.pay(&body(), TX).await;
    assert_eq!(
        app.post(&body(), Some(&credential)).await.status,
        StatusCode::OK
    );
    let replay = app.post(&body(), Some(&credential)).await;
    assert_eq!(replay.status, StatusCode::FORBIDDEN);
    assert!(replay.header("www-authenticate").is_none());
    assert_eq!(replay.header("cache-control"), Some("no-store"));
    assert_eq!(
        replay.json()["type"],
        "https://paymentauth.org/problems/verification-failed"
    );
    assert_eq!(
        app.calls.load(Ordering::SeqCst),
        1,
        "a replay never reaches the handler"
    );
    assert!(replay.header("payment-receipt").is_none());
}

#[tokio::test]
async fn a_payment_without_this_challenges_memo_is_403_and_never_claimed() {
    let app = app(true);
    let service = app.service.as_ref().unwrap();
    let challenge = service
        .challenge_for_body(CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    app.chain.mine(payment_receipt(
        TX, PAYER, CURRENCY, RECIPIENT, AMOUNT, None,
    ));
    let credential = credential_header(&challenge, hash_payload(TX), None);
    let reply = app.post(&body(), Some(&credential)).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(reply.header("www-authenticate").is_none());
    assert_eq!(app.repo.state_of(&key(TX)), None);
    assert_eq!(app.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_failed_inference_releases_the_payment_for_a_retry_of_the_same_credential() {
    let app = app(true);
    *app.mode.lock().unwrap() = Handler::Fail;
    let credential = app.pay(&body(), TX).await;
    let first = app.post(&body(), Some(&credential)).await;
    assert_eq!(first.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(app.repo.state_of(&key(TX)), Some(ClaimState::Unserved));
    assert!(
        first.header("payment-receipt").is_some(),
        "the payment itself succeeded"
    );

    *app.mode.lock().unwrap() = Handler::Ok;
    let retry = app.post(&body(), Some(&credential)).await;
    assert_eq!(
        retry.status,
        StatusCode::OK,
        "served without a second payment"
    );
    assert_eq!(app.repo.state_of(&key(TX)), Some(ClaimState::Served));
    assert_eq!(
        app.post(&body(), Some(&credential)).await.status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn streaming_responses_get_the_receipt_headers_and_stay_intact() {
    let app = app(true);
    *app.mode.lock().unwrap() = Handler::Stream;
    let credential = app.pay(&body(), TX).await;
    let reply = app.post(&body(), Some(&credential)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-type"), Some("text/event-stream"));
    assert_eq!(reply.text(), "data: one\n\ndata: [DONE]\n\n");
    assert!(reply.header("payment-receipt").is_some());
    assert_eq!(reply.header("cache-control"), Some("private"));
    assert_eq!(app.repo.state_of(&key(TX)), Some(ClaimState::Served));
}

// ----- credential errors -----

#[tokio::test]
async fn bad_credentials_get_a_402_with_a_fresh_challenge() {
    let app = app(true);
    let good = app.pay(&body(), TX).await;

    // Changed body: the digest no longer matches.
    let mut other = body();
    other.extend_from_slice(b" ");
    let reply = app.post(&other, Some(&good)).await;
    assert_eq!(reply.status, StatusCode::PAYMENT_REQUIRED);
    assert!(reply
        .header("www-authenticate")
        .unwrap()
        .starts_with("Payment "));
    assert_eq!(
        reply.json()["type"],
        "https://paymentauth.org/problems/invalid-challenge"
    );
    assert_eq!(reply.header("cache-control"), Some("no-store"));

    // Not base64, not JSON, tampered id.
    for credential in [
        "Payment !!!".to_string(),
        format!("Payment {}", protocol::b64url_encode(b"nope")),
        good.replacen(&good[10..14], "AAAA", 1),
    ] {
        let reply = app.post(&body(), Some(&credential)).await;
        assert_eq!(reply.status, StatusCode::PAYMENT_REQUIRED, "{credential}");
        assert!(reply.header("www-authenticate").is_some());
    }
    let reply = app.post(&body(), Some("Payment !!!")).await;
    assert_eq!(
        reply.json()["type"],
        "https://paymentauth.org/problems/malformed-credential"
    );
    assert_eq!(app.calls.load(Ordering::SeqCst), 0);
    assert_eq!(app.repo.state_of(&key(TX)), None);
}

#[tokio::test]
async fn an_unmined_payment_is_a_retryable_402_and_a_proof_is_a_400() {
    let app = app(true);
    let service = app.service.as_ref().unwrap();
    let challenge = service
        .challenge_for_body(CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    let credential = credential_header(&challenge, hash_payload([0x77; 32]), None);
    let reply = app.post(&body(), Some(&credential)).await;
    assert_eq!(reply.status, StatusCode::PAYMENT_REQUIRED);
    assert!(reply.header("www-authenticate").is_some());
    assert_eq!(
        reply.json()["type"],
        "https://paymentauth.org/problems/verification-failed"
    );
    assert_eq!(app.calls.load(Ordering::SeqCst), 0);

    let proof = credential_header(
        &challenge,
        json!({"type": "proof", "signature": "0x00"}),
        None,
    );
    let reply = app.post(&body(), Some(&proof)).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.json()["type"],
        "https://paymentauth.org/problems/method-unsupported"
    );
    assert!(reply.header("www-authenticate").is_none());
}

#[tokio::test]
async fn an_expired_challenge_is_payment_expired() {
    let app = app(true);
    let service = app.service.as_ref().unwrap();
    let old = Utc::now() - chrono::Duration::seconds(700);
    let challenge = service
        .challenge_at(
            old,
            CHAT_COMPLETIONS_ROUTE,
            &body(),
            &services::mpp::Price { amount: AMOUNT },
        )
        .unwrap();
    let credential = credential_header(&challenge, hash_payload(TX), None);
    let reply = app.post(&body(), Some(&credential)).await;
    assert_eq!(reply.status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(
        reply.json()["type"],
        "https://paymentauth.org/problems/payment-expired"
    );
    assert!(reply.header("www-authenticate").is_some());
}

// ----- availability -----

#[tokio::test]
async fn an_unreachable_node_is_a_503_not_a_payment_request() {
    let app = app(true);
    let credential = app.pay(&body(), TX).await;
    app.chain.set_lookup_error(Some(RpcError::Unavailable));
    let reply = app.post(&body(), Some(&credential)).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(reply.header("retry-after").is_some());
    assert!(reply.header("www-authenticate").is_none());
    assert_eq!(reply.json()["error"]["type"], "service_unavailable");
    assert_eq!(app.repo.state_of(&key(TX)), None);
    // The node comes back: the same credential works, nothing was consumed.
    app.chain.set_lookup_error(None);
    assert_eq!(
        app.post(&body(), Some(&credential)).await.status,
        StatusCode::OK
    );

    // A transaction payload whose broadcast cannot reach the node.
    let app2 = self::app(true);
    app2.chain.set_send_mode(SendMode::Unavailable);
    let service = app2.service.as_ref().unwrap();
    let challenge = service
        .challenge_for_body(CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    let (payload, _) = services::mpp::testing::transaction_payload(4);
    let credential = credential_header(&challenge, payload, None);
    assert_eq!(
        app2.post(&body(), Some(&credential)).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn the_usage_check_402_of_the_service_workspace_becomes_a_503() {
    let app = app(true);
    app.blocked.store(true, Ordering::SeqCst);
    let credential = app.pay(&body(), TX).await;
    let reply = app.post(&body(), Some(&credential)).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.header("retry-after"), Some("30"));
    assert!(reply.header("www-authenticate").is_none());
    assert!(
        !reply.text().contains("no_credits"),
        "the org's billing state is not leaked"
    );
    assert_eq!(
        app.repo.state_of(&key(TX)),
        None,
        "no payment is taken when serving is blocked"
    );
    assert_eq!(app.calls.load(Ordering::SeqCst), 0);

    // A Bearer caller still sees the ordinary 402 from the usage check.
    let reply = app
        .post(&body(), Some(&format!("Bearer {BEARER_KEY}")))
        .await;
    assert_eq!(reply.status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(reply.json()["error"]["type"], "no_credits");

    // Unblocked, the same credential is still good.
    app.blocked.store(false, Ordering::SeqCst);
    assert_eq!(
        app.post(&body(), Some(&credential)).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_misconfigured_service_key_is_a_503_and_leaks_nothing() {
    let app = app_with(true, "sk-live-not-a-known-key");
    let credential = app.pay(&body(), TX).await;
    let reply = app.post(&body(), Some(&credential)).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(reply.header("www-authenticate").is_none());
    assert_eq!(app.repo.state_of(&key(TX)), None);
    no_secrets(&reply);
    assert!(!reply.text().contains("sk-live-not-a-known-key"));
}

#[tokio::test]
async fn pending_payment_debug_prints_nothing_sensitive() {
    let app = app(true);
    let service = app.service.as_ref().unwrap();
    let challenge = service
        .challenge_for_body(CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    let credential = credential_header(&challenge, hash_payload(TX), None);
    let verified = service
        .verify(&credential, CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    let printed = format!("{:?}", PendingMppPayment::new(verified));
    assert_eq!(printed, "PendingMppPayment { .. }");
}
