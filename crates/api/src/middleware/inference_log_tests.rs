use super::*;
use async_trait::async_trait;
use axum::{
    extract::Request as AxumRequest,
    middleware::{from_fn, from_fn_with_state},
    routing::post,
    Router,
};
use chrono::Utc;
use futures::StreamExt;
use services::models::{ModelInfo, ModelWithPricing, ModelsError};
use services::organization::{Organization, OrganizationId};
use services::workspace::{ApiKey, ApiKeyId, Workspace, WorkspaceId};
use std::io;
use std::sync::Mutex;
use tower::ServiceExt;

const CATALOG_MODEL: &str = "catalog-org/served-model";
const ALIAS: &str = "served-alias";
const MARKER: &str = "TEST_CONTENT_MARKER";
const UNLISTED: &str = "catalog-org/unlisted-model";
const CALLER_KEY: &str = "sk-test-caller-key";

/// Resolves `ALIAS` and `CATALOG_MODEL` to `CATALOG_MODEL`; anything else is
/// unknown.
struct Catalog;

#[async_trait]
impl ModelsServiceTrait for Catalog {
    async fn get_models(&self) -> Result<Vec<ModelInfo>, ModelsError> {
        Ok(Vec::new())
    }
    async fn get_models_with_pricing(&self) -> Result<Vec<ModelWithPricing>, ModelsError> {
        Ok(Vec::new())
    }
    async fn get_model_by_name(
        &self,
        name: &str,
        _caller: ModelCaller,
    ) -> Result<ModelWithPricing, ModelsError> {
        Err(ModelsError::NotFound(name.to_string()))
    }
    async fn resolve_and_get_model(
        &self,
        identifier: &str,
        _caller: ModelCaller,
    ) -> Result<ModelWithPricing, ModelsError> {
        if identifier == UNLISTED {
            // Like an unlisted model: resolves only with the caller's key.
            return match services::caller_credential::current_caller_credential() {
                Some(token) if token == CALLER_KEY => Ok(model(UNLISTED)),
                _ => Err(ModelsError::NotFound(identifier.to_string())),
            };
        }
        if identifier == ALIAS || identifier == CATALOG_MODEL {
            Ok(model(CATALOG_MODEL))
        } else {
            Err(ModelsError::NotFound(identifier.to_string()))
        }
    }
    async fn resolve_public_model(
        &self,
        identifier: &str,
    ) -> Result<ModelWithPricing, ModelsError> {
        Err(ModelsError::NotFound(identifier.to_string()))
    }
    async fn resolve_alias_cached(
        &self,
        _identifier: &str,
        _caller: ModelCaller,
    ) -> Option<String> {
        None
    }
    async fn get_configured_model_names(&self) -> Result<Vec<String>, ModelsError> {
        Ok(Vec::new())
    }
    async fn invalidate_models_cache(&self) {}
}

fn model(name: &str) -> ModelWithPricing {
    ModelWithPricing {
        upcoming_pricing: None,
        id: Uuid::new_v4(),
        model_name: name.to_string(),
        model_display_name: "Served".to_string(),
        model_description: "test model".to_string(),
        model_icon: None,
        input_cost_per_token: 0,
        output_cost_per_token: 0,
        cost_per_image: 0,
        cache_read_cost_per_token: None,
        text_pricing: None,
        context_length: 16_384,
        verifiable: false,
        aliases: vec![ALIAS.to_string()],
        owned_by: "test".to_string(),
        provider_type: "vllm".to_string(),
        provider_config: None,
        attestation_supported: false,
        input_modalities: None,
        output_modalities: None,
        inference_url: None,
        hugging_face_id: None,
        quantization: None,
        max_output_length: None,
        supported_sampling_parameters: vec![],
        supported_features: vec![],
        datacenters: None,
        is_ready: Some(true),
        deprecation_date: None,
        successor_model_name: None,
        openrouter_slug: None,
        is_listed: true,
        created_at: Utc::now(),
    }
}

struct Tenant {
    request_id: Uuid,
    org_id: Uuid,
    workspace_id: Uuid,
    api_key_id: String,
}

impl Tenant {
    fn new() -> Self {
        Self {
            request_id: Uuid::new_v4(),
            org_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            api_key_id: Uuid::new_v4().to_string(),
        }
    }

    fn authenticated(&self) -> AuthenticatedApiKey {
        let owner = services::auth::UserId(Uuid::nil());
        AuthenticatedApiKey {
            api_key: ApiKey {
                id: ApiKeyId(self.api_key_id.clone()),
                key: None,
                key_prefix: "sk-test".to_string(),
                name: "k".to_string(),
                workspace_id: WorkspaceId(self.workspace_id),
                created_by_user_id: owner.clone(),
                created_at: Utc::now(),
                expires_at: None,
                last_used_at: None,
                is_active: true,
                deleted_at: None,
                spend_limit: None,
                usage: None,
            },
            workspace: Workspace {
                id: WorkspaceId(self.workspace_id),
                name: "w".to_string(),
                description: None,
                organization_id: OrganizationId(self.org_id),
                created_by_user_id: owner.clone(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
                is_active: true,
                settings: None,
            },
            organization: Organization {
                request_priority: 0,
                id: OrganizationId(self.org_id),
                name: "o".to_string(),
                description: None,
                owner_id: owner,
                settings: serde_json::json!({}),
                is_active: true,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
        }
    }
}

/// `route` behind the log layer, with a stand-in for authentication that
/// adds the tenant's key and request id.
fn app(tenant: &Tenant, route: axum::routing::MethodRouter) -> Router {
    let authenticated = tenant.authenticated();
    let request_id = tenant.request_id;
    Router::new()
        .route("/v1/test", route)
        .layer(from_fn_with_state(
            InferenceLogState {
                models_service: Arc::new(Catalog),
            },
            inference_log_middleware,
        ))
        .layer(from_fn(move |mut request: AxumRequest, next: Next| {
            let authenticated = authenticated.clone();
            async move {
                request.extensions_mut().insert(authenticated);
                request
                    .extensions_mut()
                    .insert(RequestCorrelation { request_id });
                next.run(request).await
            }
        }))
}

fn request() -> AxumRequest {
    Request::builder()
        .method("POST")
        .uri("/v1/test")
        .header("x-org-id", "client-org-value")
        .body(Body::from(format!(r#"{{"prompt":"{MARKER}"}}"#)))
        .expect("test request should build")
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

struct LogsWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for LogsWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync {
        let logs = self.0.clone();
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(move || LogsWriter(logs.clone()))
            .finish()
    }

    /// Wait for the line (the model lookup runs on a spawned task).
    async fn line(&self, message: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let text = String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned();
            if let Some(line) = text.lines().find(|line| line.contains(message)) {
                return line.to_string();
            }
            assert!(Instant::now() < deadline, "no `{message}` line in:\n{text}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn count(&self, message: &str) -> usize {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter(|line| line.contains(message))
            .count()
    }
}

fn assert_ids(line: &str, tenant: &Tenant) {
    for expected in [
        format!("request_id={}", tenant.request_id),
        format!("org_id={}", tenant.org_id),
        format!("workspace_id={}", tenant.workspace_id),
        format!("api_key_id={}", tenant.api_key_id),
        "path=/v1/test".to_string(),
    ] {
        assert!(line.contains(&expected), "missing {expected} in: {line}");
    }
    for unexpected in [MARKER, "client-org-value"] {
        assert!(
            !line.contains(unexpected),
            "unexpected {unexpected} in: {line}"
        );
    }
}

#[tokio::test]
async fn a_finished_request_logs_ids_resolved_model_status_and_duration() {
    let logs = Logs::default();
    let _guard = tracing::subscriber::set_default(logs.subscriber());
    let tenant = Tenant::new();
    let app = app(
        &tenant,
        post(|requested: RequestedModel| async move {
            requested.set(ALIAS);
            (axum::http::StatusCode::CREATED, "done")
        }),
    );

    let response = app.oneshot(request()).await.unwrap();
    assert_eq!(response.status().as_u16(), 201);
    // Give a line written at handler return (and its model lookup) time to
    // show up: none may appear until the body has been read.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        logs.count("inference request"),
        0,
        "logged before the body ended"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], b"done");

    let line = logs.line("inference request finished").await;
    assert_ids(&line, &tenant);
    assert!(
        line.contains(&format!("model=\"{CATALOG_MODEL}\"")),
        "{line}"
    );
    assert!(line.contains("status=201"), "{line}");
    assert!(line.contains("duration_ms="), "{line}");
    assert!(
        !line.contains(&format!("\"{ALIAS}\"")),
        "alias in place of the catalog id: {line}"
    );
    assert_eq!(logs.count("inference request"), 1);
}

#[tokio::test]
async fn an_unresolved_model_string_is_never_logged() {
    let logs = Logs::default();
    let _guard = tracing::subscriber::set_default(logs.subscriber());
    let tenant = Tenant::new();
    let app = app(
        &tenant,
        post(|requested: RequestedModel| async move {
            requested.set(MARKER);
            (axum::http::StatusCode::NOT_FOUND, "no such model")
        }),
    );

    let response = app.oneshot(request()).await.unwrap();
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX).await;

    let line = logs.line("inference request finished").await;
    assert_ids(&line, &tenant);
    assert!(line.contains("status=404"), "{line}");
    assert!(!line.contains("model="), "{line}");
}

#[tokio::test]
async fn a_client_that_disconnects_mid_stream_is_logged_as_cancelled() {
    let logs = Logs::default();
    let _guard = tracing::subscriber::set_default(logs.subscriber());
    let tenant = Tenant::new();
    let app = app(
        &tenant,
        post(|requested: RequestedModel| async move {
            requested.set(CATALOG_MODEL);
            let chunks = futures::stream::iter([Ok::<_, io::Error>(Bytes::from("data: 1\n\n"))])
                .chain(futures::stream::pending());
            Body::from_stream(chunks)
        }),
    );

    let response = app.oneshot(request()).await.unwrap();
    let mut body = response.into_body().into_data_stream();
    assert!(body.next().await.is_some(), "first chunk");
    // The client goes away with the stream still open.
    drop(body);

    let line = logs.line("inference request cancelled").await;
    assert_ids(&line, &tenant);
    assert!(line.contains("status=200"), "{line}");
    assert!(line.contains("stage=\"during_body\""), "{line}");
    assert!(
        line.contains(&format!("model=\"{CATALOG_MODEL}\"")),
        "{line}"
    );
    assert_eq!(logs.count("inference request finished"), 0);
}

#[tokio::test]
async fn a_request_dropped_before_its_response_is_logged_as_cancelled() {
    let logs = Logs::default();
    let _guard = tracing::subscriber::set_default(logs.subscriber());
    let tenant = Tenant::new();
    let app = app(
        &tenant,
        post(|| async {
            std::future::pending::<()>().await;
            "unreachable"
        }),
    );

    let pending = app.oneshot(request());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), pending)
            .await
            .is_err(),
        "handler should still be waiting"
    );

    let line = logs.line("inference request cancelled").await;
    assert_ids(&line, &tenant);
    assert!(line.contains("stage=\"before_response\""), "{line}");
    assert!(!line.contains("status="), "{line}");
}

#[tokio::test]
async fn a_request_without_an_authenticated_key_is_not_logged() {
    let logs = Logs::default();
    let _guard = tracing::subscriber::set_default(logs.subscriber());
    let app = Router::new()
        .route("/v1/test", post(|| async { "ok" }))
        .layer(from_fn_with_state(
            InferenceLogState {
                models_service: Arc::new(Catalog),
            },
            inference_log_middleware,
        ));

    let response = app.oneshot(request()).await.unwrap();
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX).await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    assert_eq!(logs.count("inference request"), 0);
}

#[tokio::test]
async fn tenant_ids_recorded_after_auth_appear_on_lines_logged_inside_the_request() {
    let logs = Logs::default();
    let writer = logs.0.clone();
    let subscriber = tracing_subscriber::fmt()
        .compact()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(move || LogsWriter(writer.clone()))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let tenant = Tenant::new();
    let authenticated = tenant.authenticated();
    let app = Router::new()
        .route(
            "/v1/test",
            post(|| async {
                tracing::info!("handler line");
                "ok"
            }),
        )
        .layer(from_fn(move |request: AxumRequest, next: Next| {
            let authenticated = authenticated.clone();
            async move {
                crate::middleware::auth::record_tenant_on_span(&authenticated);
                next.run(request).await
            }
        }))
        .layer(from_fn(
            crate::middleware::request_correlation::request_correlation_middleware,
        ));

    let response = app.oneshot(request()).await.unwrap();
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX).await;

    let line = logs.line("handler line").await;
    for expected in [
        format!("org_id={}", tenant.org_id),
        format!("workspace_id={}", tenant.workspace_id),
        format!("api_key_id={}", tenant.api_key_id),
    ] {
        assert!(line.contains(&expected), "missing {expected} in: {line}");
    }
}

#[tokio::test]
async fn a_client_error_skips_the_model_lookup() {
    let logs = Logs::default();
    let _guard = tracing::subscriber::set_default(logs.subscriber());
    let tenant = Tenant::new();
    let app = app(
        &tenant,
        post(|requested: RequestedModel| async move {
            // Would resolve, but a 4xx answer skips the lookup.
            requested.set(ALIAS);
            (axum::http::StatusCode::BAD_REQUEST, "bad")
        }),
    );

    let response = app.oneshot(request()).await.unwrap();
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX).await;

    let line = logs.line("inference request finished").await;
    assert!(line.contains("status=400"), "{line}");
    assert!(!line.contains("model="), "{line}");
}

#[tokio::test]
async fn the_deferred_lookup_runs_with_the_callers_key() {
    let logs = Logs::default();
    let _guard = tracing::subscriber::set_default(logs.subscriber());
    let tenant = Tenant::new();
    let app = app(
        &tenant,
        post(|requested: RequestedModel| async move {
            requested.set(UNLISTED);
            "ok"
        }),
    )
    // Authentication puts the caller's key in scope outside the log layer.
    .layer(from_fn(|request: AxumRequest, next: Next| async move {
        services::caller_credential::with_caller_credential(
            CALLER_KEY.to_string(),
            next.run(request),
        )
        .await
    }));

    let response = app.oneshot(request()).await.unwrap();
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX).await;

    let line = logs.line("inference request finished").await;
    assert!(line.contains(&format!("model=\"{UNLISTED}\"")), "{line}");
    assert!(!line.contains(CALLER_KEY), "unexpected key in: {line}");
}
