//! The audit layer under the production fallbacks and layer stack
//! (`with_app_fallbacks_and_layers`) and the real `admin_middleware`, with
//! mock session auth and no database. Handlers are probes standing in for the
//! admin routes; the e2e suite covers the real admin routers.

use super::*;
use crate::middleware::{
    auth::{admin_middleware, AuthState},
    request_correlation::REQUEST_ID_HEADER,
    MetricsState,
};
use axum::{
    http::{
        header::{
            ACCESS_CONTROL_REQUEST_METHOD, AUTHORIZATION, CACHE_CONTROL, COOKIE, ORIGIN, USER_AGENT,
        },
        HeaderMap, Method, StatusCode,
    },
    middleware::from_fn_with_state,
    routing::get,
    Router,
};
use serde_json::{Map, Value};
use services::auth::{AuthServiceTrait, MockAuthService, SessionId, UserId};
use std::collections::BTreeSet;
use std::io;
use tokio::sync::Notify;
use tower::ServiceExt;
use tracing::subscriber::DefaultGuard;

const ENCODING_KEY: &str = "admin-audit-test-encoding-key";
const ADMIN_EMAILS: &[&str] = &["admin@test.com"];
/// The mock auth service authenticates every user as admin@test.com.
const NO_ADMIN_EMAILS: &[&str] = &["other@test.com"];
const PROBE_LIMIT: i64 = 2;
const PROBE_OFFSET: i64 = 4;
const PROBE_RETURNED: usize = 3;
const FILTER_ORGANIZATION_ID: Uuid = Uuid::from_u128(0x0f11_7e40);

const EVENT_FIELDS: &[&str] = &[
    "message",
    "audit_event",
    "request_id",
    "admin_route",
    "admin_method",
    "admin_status",
    "admin_auth_outcome",
    "admin_user_id",
    "admin_credential_kind",
    "admin_credential_id",
    "admin_organization_id",
    "admin_limit",
    "admin_offset",
    "admin_returned_count",
    "admin_duration_ms",
];

#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

struct CapturedLogsWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for CapturedLogsWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("captured logs mutex should not poison")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl CapturedLogs {
    /// JSON lines without span fields, as production writes them.
    fn json() -> (Self, DefaultGuard) {
        let logs = Self::default();
        let writer = logs.0.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_current_span(false)
            .with_span_list(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || CapturedLogsWriter(writer.clone()))
            .finish();
        (logs, tracing::subscriber::set_default(subscriber))
    }

    /// The compact format, which prints the enclosing spans' fields.
    fn compact() -> (Self, DefaultGuard) {
        let logs = Self::default();
        let writer = logs.0.clone();
        let subscriber = tracing_subscriber::fmt()
            .compact()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || CapturedLogsWriter(writer.clone()))
            .finish();
        (logs, tracing::subscriber::set_default(subscriber))
    }

    /// Everything logged so far, emptying the buffer.
    fn take(&self) -> String {
        let mut logs = self
            .0
            .lock()
            .expect("captured logs mutex should not poison");
        String::from_utf8(std::mem::take(&mut *logs)).expect("logs are UTF-8")
    }

    /// Audit events logged so far (their `fields`), emptying the buffer.
    fn take_events(&self) -> Vec<Map<String, Value>> {
        self.take()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("JSON log line"))
            .filter(|line| line["fields"]["audit_event"] == ADMIN_AUDIT_EVENT)
            .map(|line| {
                let keys: BTreeSet<_> = line
                    .as_object()
                    .expect("log line is an object")
                    .keys()
                    .map(String::as_str)
                    .collect();
                assert_eq!(
                    keys,
                    BTreeSet::from(["fields", "level", "target", "timestamp"]),
                    "audit events carry no span data: {line}"
                );
                assert_eq!(line["level"], "INFO");
                let fields = line["fields"].as_object().cloned().expect("fields object");
                for key in fields.keys() {
                    assert!(
                        EVENT_FIELDS.contains(&key.as_str()),
                        "unexpected field {key}"
                    );
                }
                fields
            })
            .collect()
    }

    fn take_single_event(&self) -> Map<String, Value> {
        let mut events = self.take_events();
        assert_eq!(events.len(), 1, "one audit event per request: {events:?}");
        events.remove(0)
    }
}

fn auth_state(admin_emails: &[&str]) -> AuthState {
    let pool = database::DbPool::uninitialized();
    AuthState::new(
        Arc::new(services::auth::OAuthManager::new(None, None).expect("no OAuth providers")),
        Arc::new(MockAuthService {
            apikey_repository: Arc::new(database::repositories::ApiKeyRepository::new(
                pool.clone(),
            )),
        }),
        Arc::new(database::repositories::WorkspaceRepository::new(
            pool.clone(),
        )),
        Arc::new(
            services::reporting_tokens::OrganizationReportingTokenServiceImpl::new(Arc::new(
                database::repositories::OrganizationReportingTokenRepository::new(pool.clone()),
            )),
        ),
        // No database: validating an admin access token fails as a 500.
        Arc::new(database::repositories::AdminAccessTokenRepository::new(
            pool,
        )),
        admin_emails.iter().map(|email| email.to_string()).collect(),
        ENCODING_KEY.to_string(),
    )
}

fn admin_tokens_only_app(admin_tokens_only: bool) -> Router {
    Router::new()
        .route("/v1/admin/probe", get(probe))
        .layer(from_fn_with_state(
            auth_state(ADMIN_EMAILS).with_admin_tokens_only(admin_tokens_only),
            admin_middleware,
        ))
}

fn session_token(user_id: Uuid, session_id: Option<Uuid>) -> String {
    MockAuthService {
        apikey_repository: Arc::new(database::repositories::ApiKeyRepository::new(
            database::DbPool::uninitialized(),
        )),
    }
    .create_session_access_token(
        UserId(user_id),
        session_id.map(SessionId),
        ENCODING_KEY.to_string(),
        1,
    )
    .expect("mock auth signs session tokens")
}

async fn probe(headers: HeaderMap) -> &'static str {
    if headers.contains_key(ADMIN_AUDIT_REQUEST_ID_HEADER) {
        "client audit id reached the handler"
    } else {
        "ok"
    }
}

async fn members_probe(audit: AdminAudit) -> &'static str {
    audit.record_page(PROBE_LIMIT, PROBE_OFFSET, PROBE_RETURNED);
    "ok"
}

async fn api_keys_probe(audit: AdminAudit) -> &'static str {
    audit.record_organization(FILTER_ORGANIZATION_ID);
    "ok"
}

fn app(admin_emails: &[&str], entered_slow_handler: Arc<Notify>) -> Router {
    let admin_routes = Router::new()
        .route("/admin/probe", get(probe))
        .route("/admin/api-keys", get(api_keys_probe))
        .route("/admin/organizations/{org_id}", get(|| async { "ok" }))
        .route("/admin/organizations/{org_id}/members", get(members_probe))
        .route(
            "/admin/slow",
            get(move || {
                let entered = entered_slow_handler.clone();
                async move {
                    entered.notify_one();
                    std::future::pending::<&'static str>().await
                }
            }),
        )
        .layer(from_fn_with_state(
            auth_state(admin_emails),
            admin_middleware,
        ));
    let routes = Router::new().nest(
        "/v1",
        Router::new()
            .merge(admin_routes)
            .route("/health", get(|| async { "ok" })),
    );
    crate::with_app_fallbacks_and_layers(
        routes,
        MetricsState {
            metrics_service: Arc::new(services::metrics::MockMetricsService),
        },
        crate::build_cors_layer(config::CorsConfig::default()),
    )
}

fn admin_app() -> Router {
    app(ADMIN_EMAILS, Arc::new(Notify::new()))
}

fn request(method: Method, uri: &str) -> axum::http::request::Builder {
    Request::builder().method(method).uri(uri)
}

fn with_session(
    builder: axum::http::request::Builder,
    user_id: Uuid,
    session_id: Option<Uuid>,
) -> Request<Body> {
    builder
        .header(
            AUTHORIZATION,
            format!("Bearer {}", session_token(user_id, session_id)),
        )
        .body(Body::empty())
        .expect("test request")
}

async fn send(app: &Router, request: Request<Body>) -> Response {
    app.clone()
        .oneshot(request)
        .await
        .expect("the router is infallible")
}

fn header(response: &Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_else(|| panic!("response has {name}"))
        .to_string()
}

async fn body_text(response: Response) -> String {
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads");
    String::from_utf8(body.to_vec()).expect("UTF-8 body")
}

fn assert_absent(event: &Map<String, Value>, fields: &[&str]) {
    for field in fields {
        assert!(
            !event.contains_key(*field),
            "{field} must be omitted: {event:?}"
        );
    }
}

#[test]
fn admin_paths_are_the_admin_api_only() {
    for path in [
        "/v1/admin",
        "/v1/admin/",
        "/v1/admin/probe",
        "/v1/admin/x/y",
    ] {
        assert!(is_admin_path(path), "{path}");
    }
    for path in [
        "/v1/administrator",
        "/v1/admin-probe",
        "/admin/probe",
        "/v2/admin/probe",
        "/v1/organizations/admin",
        "/V1/ADMIN/probe",
    ] {
        assert!(!is_admin_path(path), "{path}");
    }
}

#[tokio::test]
async fn audit_header_carries_the_server_request_id_never_a_client_value() {
    let (logs, _guard) = CapturedLogs::json();
    let app = admin_app();
    let client_request_id = Uuid::new_v4().to_string();
    let spoofed_audit_id = Uuid::new_v4().to_string();

    let response = send(
        &app,
        with_session(
            request(Method::GET, "/v1/admin/probe")
                .header(REQUEST_ID_HEADER, &client_request_id)
                .header(ADMIN_AUDIT_REQUEST_ID_HEADER, &spoofed_audit_id),
            Uuid::new_v4(),
            Some(Uuid::new_v4()),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    // x-request-id still echoes the client; the audit header never does.
    assert_eq!(header(&response, REQUEST_ID_HEADER), client_request_id);
    let audit_id = header(&response, ADMIN_AUDIT_REQUEST_ID_HEADER);
    assert!(Uuid::parse_str(&audit_id).is_ok());
    assert_ne!(audit_id, client_request_id);
    assert_ne!(audit_id, spoofed_audit_id);
    assert_eq!(header(&response, CACHE_CONTROL.as_str()), "no-store");
    assert_eq!(body_text(response).await, "ok");
    assert_eq!(logs.take_single_event()["request_id"], audit_id.as_str());

    // Without a client id, x-request-id is the correlation middleware's own
    // id, and the audit header is that same id.
    let response = send(
        &app,
        with_session(
            request(Method::GET, "/v1/admin/probe"),
            Uuid::new_v4(),
            None,
        ),
    )
    .await;
    let audit_id = header(&response, ADMIN_AUDIT_REQUEST_ID_HEADER);
    assert_eq!(audit_id, header(&response, REQUEST_ID_HEADER));
    assert_eq!(logs.take_single_event()["request_id"], audit_id.as_str());
}

#[tokio::test]
async fn allowed_session_read_records_identity_route_organization_and_page() {
    let (logs, _guard) = CapturedLogs::json();
    let app = admin_app();
    let (user_id, session_id, organization_id) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());

    let response = send(
        &app,
        with_session(
            request(
                Method::GET,
                &format!("/v1/admin/organizations/{organization_id}/members?limit=2&offset=4"),
            ),
            user_id,
            Some(session_id),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let event = logs.take_single_event();
    assert_eq!(event["message"], "admin request");
    assert_eq!(
        event["admin_route"],
        "/v1/admin/organizations/{org_id}/members"
    );
    assert_eq!(event["admin_method"], "GET");
    assert_eq!(event["admin_status"], 200);
    assert_eq!(event["admin_auth_outcome"], "allowed");
    assert_eq!(event["admin_credential_kind"], "session");
    assert_eq!(event["admin_credential_id"], session_id.to_string());
    assert_eq!(event["admin_user_id"], user_id.to_string());
    assert_eq!(event["admin_organization_id"], organization_id.to_string());
    assert_eq!(event["admin_limit"], PROBE_LIMIT);
    assert_eq!(event["admin_offset"], PROBE_OFFSET);
    assert_eq!(event["admin_returned_count"], PROBE_RETURNED);
    assert!(event["admin_duration_ms"].is_u64());
    // A fully populated event carries every contract field.
    assert_eq!(
        event.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        EVENT_FIELDS.iter().copied().collect()
    );
}

#[tokio::test]
async fn global_reads_have_no_organization_unless_a_handler_records_a_filter() {
    let (logs, _guard) = CapturedLogs::json();
    let app = admin_app();
    let user_id = Uuid::new_v4();

    // A legacy session token has no `sid` claim: the credential id is unknown.
    let response = send(
        &app,
        with_session(request(Method::GET, "/v1/admin/probe"), user_id, None),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let event = logs.take_single_event();
    assert_eq!(event["admin_auth_outcome"], "allowed");
    assert_eq!(event["admin_credential_kind"], "session");
    assert_eq!(event["admin_user_id"], user_id.to_string());
    assert_absent(
        &event,
        &[
            "admin_credential_id",
            "admin_organization_id",
            "admin_limit",
            "admin_offset",
            "admin_returned_count",
        ],
    );

    let response = send(
        &app,
        with_session(
            request(Method::GET, "/v1/admin/api-keys"),
            user_id,
            Some(Uuid::new_v4()),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        logs.take_single_event()["admin_organization_id"],
        FILTER_ORGANIZATION_ID.to_string()
    );
}

#[tokio::test]
async fn authenticated_user_denied_by_policy_keeps_identity_but_no_organization() {
    let (logs, _guard) = CapturedLogs::json();
    let app = app(NO_ADMIN_EMAILS, Arc::new(Notify::new()));
    let (user_id, session_id) = (Uuid::new_v4(), Uuid::new_v4());

    let response = send(
        &app,
        with_session(
            request(
                Method::GET,
                &format!("/v1/admin/organizations/{}", Uuid::new_v4()),
            ),
            user_id,
            Some(session_id),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(response
        .headers()
        .contains_key(ADMIN_AUDIT_REQUEST_ID_HEADER));
    let event = logs.take_single_event();
    assert_eq!(event["admin_route"], "/v1/admin/organizations/{org_id}");
    assert_eq!(event["admin_status"], 403);
    assert_eq!(event["admin_auth_outcome"], "denied");
    assert_eq!(event["admin_credential_kind"], "session");
    assert_eq!(event["admin_credential_id"], session_id.to_string());
    assert_eq!(event["admin_user_id"], user_id.to_string());
    assert_absent(&event, &["admin_organization_id"]);
}

#[tokio::test]
async fn rejected_or_missing_credentials_are_denied_without_identity() {
    let (logs, _guard) = CapturedLogs::json();
    let app = admin_app();
    let path = format!("/v1/admin/organizations/{}", Uuid::new_v4());
    let cases = [
        (None, StatusCode::UNAUTHORIZED, "none"),
        (
            Some("Basic dXNlcjpwYXNz".to_string()),
            StatusCode::UNAUTHORIZED,
            "none",
        ),
        (
            Some(format!(
                "Bearer {}rejected-by-session-auth",
                services::common::REPORTING_TOKEN_PREFIX
            )),
            StatusCode::UNAUTHORIZED,
            "session",
        ),
        // The token store is unavailable, so the token never validates.
        (
            Some("Bearer adm_unvalidated_admin_token".to_string()),
            StatusCode::INTERNAL_SERVER_ERROR,
            "admin_access_token",
        ),
    ];

    for (authorization, status, credential_kind) in cases {
        let mut builder = request(Method::GET, &path);
        if let Some(authorization) = authorization {
            builder = builder.header(AUTHORIZATION, authorization);
        }
        let response = send(&app, builder.body(Body::empty()).expect("test request")).await;

        assert_eq!(response.status(), status);
        let audit_id = header(&response, ADMIN_AUDIT_REQUEST_ID_HEADER);
        let event = logs.take_single_event();
        assert_eq!(event["request_id"], audit_id.as_str());
        assert_eq!(event["admin_route"], "/v1/admin/organizations/{org_id}");
        assert_eq!(event["admin_status"], status.as_u16());
        assert_eq!(event["admin_auth_outcome"], "denied");
        assert_eq!(event["admin_credential_kind"], credential_kind);
        assert_absent(
            &event,
            &[
                "admin_user_id",
                "admin_credential_id",
                "admin_organization_id",
            ],
        );
    }
}

#[tokio::test]
async fn admin_requests_outside_admin_auth_are_audited_as_unknown() {
    let (logs, _guard) = CapturedLogs::json();
    let app = admin_app();

    for path in ["/v1/admin/no-such-route", "/v1/admin", "/v1/admin/probe/"] {
        let response = send(
            &app,
            with_session(request(Method::GET, path), Uuid::new_v4(), None),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        assert!(response
            .headers()
            .contains_key(ADMIN_AUDIT_REQUEST_ID_HEADER));
        let event = logs.take_single_event();
        assert_eq!(event["admin_route"], UNMATCHED_ROUTE, "{path}");
        assert_eq!(event["admin_status"], 404);
        assert_eq!(event["admin_auth_outcome"], "unknown");
        assert_eq!(event["admin_credential_kind"], "none");
        assert_absent(&event, &["admin_user_id", "admin_credential_id"]);
    }

    let response = send(
        &app,
        request(Method::OPTIONS, "/v1/admin/probe")
            .header(ORIGIN, "http://localhost:3000")
            .header(ACCESS_CONTROL_REQUEST_METHOD, "GET")
            .body(Body::empty())
            .expect("test request"),
    )
    .await;
    assert!(response.status().is_success());
    assert!(response
        .headers()
        .contains_key(ADMIN_AUDIT_REQUEST_ID_HEADER));
    let event = logs.take_single_event();
    assert_eq!(event["admin_route"], "/v1/admin/probe");
    assert_eq!(event["admin_method"], "OPTIONS");
    assert_eq!(event["admin_auth_outcome"], "unknown");
    assert_eq!(event["admin_credential_kind"], "none");
}

#[tokio::test]
async fn unsupported_method_on_an_admin_route_is_audited_once() {
    let (logs, _guard) = CapturedLogs::json();
    let app = admin_app();

    let response = send(
        &app,
        with_session(
            request(Method::DELETE, "/v1/admin/probe"),
            Uuid::new_v4(),
            None,
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    let event = logs.take_single_event();
    assert_eq!(event["admin_route"], "/v1/admin/probe");
    assert_eq!(event["admin_method"], "DELETE");
    assert_eq!(event["admin_status"], 405);
}

#[tokio::test]
async fn non_admin_requests_are_not_audited() {
    let (logs, _guard) = CapturedLogs::json();
    let app = admin_app();

    for (path, status) in [
        ("/v1/health", StatusCode::OK),
        ("/v1/administrator", StatusCode::NOT_FOUND),
        ("/v1/admin-probe", StatusCode::NOT_FOUND),
    ] {
        let response = send(
            &app,
            request(Method::GET, path)
                .header(ADMIN_AUDIT_REQUEST_ID_HEADER, Uuid::new_v4().to_string())
                .body(Body::empty())
                .expect("test request"),
        )
        .await;
        assert_eq!(response.status(), status, "{path}");
        assert!(!response
            .headers()
            .contains_key(ADMIN_AUDIT_REQUEST_ID_HEADER));
    }
    assert!(logs.take_events().is_empty());
}

#[tokio::test]
async fn every_request_emits_exactly_one_event_with_its_own_id() {
    let (logs, _guard) = CapturedLogs::json();
    let allowed = admin_app();
    let denied = app(NO_ADMIN_EMAILS, Arc::new(Notify::new()));
    let mut audit_ids = Vec::new();

    for (app, request) in [
        (
            &allowed,
            with_session(
                request(Method::GET, "/v1/admin/probe"),
                Uuid::new_v4(),
                None,
            ),
        ),
        (
            &denied,
            with_session(
                request(Method::GET, "/v1/admin/probe"),
                Uuid::new_v4(),
                None,
            ),
        ),
        (
            &allowed,
            request(Method::GET, "/v1/admin/probe")
                .body(Body::empty())
                .expect("test request"),
        ),
        (
            &allowed,
            request(Method::GET, "/v1/admin/unknown")
                .body(Body::empty())
                .expect("test request"),
        ),
        (
            &allowed,
            with_session(
                request(Method::GET, "/v1/admin/api-keys"),
                Uuid::new_v4(),
                Some(Uuid::new_v4()),
            ),
        ),
    ] {
        let response = send(app, request).await;
        audit_ids.push(header(&response, ADMIN_AUDIT_REQUEST_ID_HEADER));
    }

    let events = logs.take_events();
    let logged_ids: Vec<_> = events
        .iter()
        .map(|event| {
            event["request_id"]
                .as_str()
                .expect("request_id")
                .to_string()
        })
        .collect();
    assert_eq!(logged_ids, audit_ids);
    assert_eq!(
        logged_ids.iter().collect::<BTreeSet<_>>().len(),
        audit_ids.len(),
        "request ids are unique"
    );
}

#[tokio::test]
async fn request_dropped_before_a_response_is_audited_once_without_status() {
    let (logs, _guard) = CapturedLogs::json();
    let entered = Arc::new(Notify::new());
    let app = app(ADMIN_EMAILS, entered.clone());
    let user_id = Uuid::new_v4();
    let pending = send(
        &app,
        with_session(request(Method::GET, "/v1/admin/slow"), user_id, None),
    );

    tokio::select! {
        _ = pending => panic!("the slow handler never responds"),
        // Leaving the select drops the in-flight request, as a disconnect does.
        _ = entered.notified() => {}
    }

    let event = logs.take_single_event();
    assert_eq!(event["admin_route"], "/v1/admin/slow");
    assert_eq!(event["admin_auth_outcome"], "allowed");
    assert_eq!(event["admin_user_id"], user_id.to_string());
    assert!(event["admin_duration_ms"].is_u64());
    assert_absent(&event, &["admin_status"]);
}

#[tokio::test]
async fn audit_events_exclude_credentials_content_and_client_controlled_values() {
    const EMAIL: &str = "audit-sentinel-person@customer.example";
    const QUERY: &str = "QUERY_SENTINEL";
    const BODY: &str = "BODY_SENTINEL";
    const USER_AGENT_VALUE: &str = "USER_AGENT_SENTINEL/1.0";
    const FORWARDED_IP: &str = "203.0.113.77";
    const COOKIE_VALUE: &str = "COOKIE_SENTINEL";
    const ADMIN_TOKEN: &str = "adm_ADMIN_TOKEN_SENTINEL";
    let (logs, _guard) = CapturedLogs::json();
    let app = admin_app();
    let session = session_token(Uuid::new_v4(), Some(Uuid::new_v4()));
    let client_request_id = Uuid::new_v4().to_string();
    let sentinel_request = |method: Method, uri: String, authorization: &str| {
        request(method, &uri)
            .header(AUTHORIZATION, format!("Bearer {authorization}"))
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header("x-forwarded-for", FORWARDED_IP)
            .header("x-real-ip", FORWARDED_IP)
            .header(COOKIE, format!("session={COOKIE_VALUE}"))
            .header(REQUEST_ID_HEADER, &client_request_id)
            .body(Body::from(format!(
                r#"{{"email":"{EMAIL}","note":"{BODY}"}}"#
            )))
            .expect("test request")
    };

    let organization_id = Uuid::new_v4();
    for request in [
        sentinel_request(
            Method::GET,
            format!("/v1/admin/organizations/{organization_id}/members?search={EMAIL}&q={QUERY}"),
            &session,
        ),
        sentinel_request(
            Method::GET,
            format!("/v1/admin/probe/{EMAIL}/{QUERY}"),
            &session,
        ),
        sentinel_request(
            Method::GET,
            format!("/v1/admin/probe?q={QUERY}"),
            ADMIN_TOKEN,
        ),
    ] {
        send(&app, request).await;
    }

    let logs = logs.take();
    let audit_lines: Vec<_> = logs
        .lines()
        .filter(|line| line.contains(ADMIN_AUDIT_EVENT))
        .collect();
    assert_eq!(audit_lines.len(), 3, "{logs}");
    let info_lines: Vec<_> = logs
        .lines()
        .filter(|line| {
            let level =
                serde_json::from_str::<Value>(line).expect("JSON log line")["level"].clone();
            level != "DEBUG" && level != "TRACE"
        })
        .collect();
    for forbidden in [
        EMAIL,
        QUERY,
        BODY,
        USER_AGENT_VALUE,
        FORWARDED_IP,
        COOKIE_VALUE,
        ADMIN_TOKEN,
        "ADMIN_TOKEN_SENTINEL",
        session.as_str(),
        client_request_id.as_str(),
    ] {
        for line in audit_lines.iter().chain(&info_lines) {
            assert!(!line.contains(forbidden), "{forbidden} leaked: {line}");
        }
    }
    // The org-scoped route still identifies the organization, from its template.
    assert!(audit_lines[0].contains(&organization_id.to_string()));
    assert!(audit_lines[1].contains(UNMATCHED_ROUTE));
}

#[tokio::test]
async fn audit_event_is_a_root_event_even_when_the_format_prints_spans() {
    let (logs, _guard) = CapturedLogs::compact();
    let app = admin_app();
    let email = "span-sentinel@customer.example";
    let client_request_id = Uuid::new_v4().to_string();

    send(
        &app,
        with_session(
            request(Method::GET, &format!("/v1/admin/probe/{email}"))
                .header(REQUEST_ID_HEADER, &client_request_id),
            Uuid::new_v4(),
            None,
        ),
    )
    .await;

    let logs = logs.take();
    let audit_line = logs
        .lines()
        .find(|line| line.contains(ADMIN_AUDIT_EVENT))
        .unwrap_or_else(|| panic!("an audit event: {logs}"));
    // The request span records the raw path and the client's id; the format
    // may name the span, but none of its fields reach the audit event.
    for span_value in [email, client_request_id.as_str(), "path="] {
        assert!(!audit_line.contains(span_value), "{audit_line}");
    }
}

#[tokio::test]
async fn admin_tokens_only_refuses_session_tokens() {
    let app = admin_tokens_only_app(true);

    let response = send(
        &app,
        with_session(
            request(Method::GET, "/v1/admin/probe"),
            Uuid::new_v4(),
            Some(Uuid::new_v4()),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = serde_json::from_str(&body_text(response).await).expect("JSON body");
    assert_eq!(body["error"]["message"], "Admin access token required");
    assert_eq!(body["error"]["type"], "unauthorized");
}

#[tokio::test]
async fn admin_tokens_only_still_validates_admin_access_tokens() {
    let app = admin_tokens_only_app(true);

    // The token store is unavailable, so validation is reached and fails as a
    // 500 rather than the session refusal.
    let response = send(
        &app,
        request(Method::GET, "/v1/admin/probe")
            .header(AUTHORIZATION, "Bearer adm_unvalidated_admin_token")
            .body(Body::empty())
            .expect("test request"),
    )
    .await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body: Value = serde_json::from_str(&body_text(response).await).expect("JSON body");
    assert_eq!(
        body["error"]["message"],
        "Failed to validate admin access token"
    );
}

#[tokio::test]
async fn session_tokens_are_accepted_when_admin_tokens_only_is_off() {
    let app = admin_tokens_only_app(false);

    let response = send(
        &app,
        with_session(
            request(Method::GET, "/v1/admin/probe"),
            Uuid::new_v4(),
            Some(Uuid::new_v4()),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "ok");
}
