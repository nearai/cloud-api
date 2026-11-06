//! Admin audit events on the real router: one per request for every mounted
//! admin route, with identities, pages and organizations recorded by the real
//! auth middleware and handlers.

use crate::common::*;
use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    response::Response,
    Router,
};
use serde_json::{json, Map, Value};
use services::auth::{AuthServiceTrait, MockAuthService, SessionId, UserId};
use std::collections::BTreeSet;
use std::io;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use tracing::subscriber::DefaultGuard;
use utoipa::OpenApi;
use uuid::Uuid;

const AUDIT_HEADER: &str = "x-admin-audit-request-id";
const REQUEST_ID_HEADER: &str = "x-request-id";

/// Every admin route the app mounts: `build_admin_routes`, the admin
/// feature-request route, the database-encryption routes (mounted because
/// the e2e configuration has a valid encryption key), and the object-encryption
/// router (using mock storage).
const MOUNTED_ADMIN_ROUTES: &[(&str, &str)] = &[
    ("GET", "/admin/models"),
    ("PATCH", "/admin/models"),
    ("POST", "/admin/models/deprecate"),
    ("GET", "/admin/models/pricing-changes"),
    ("POST", "/admin/models/pricing-changes/preview"),
    ("POST", "/admin/models/pricing-changes/confirm"),
    ("DELETE", "/admin/models/pricing-changes/{id}"),
    ("DELETE", "/admin/models/{model_name}"),
    ("GET", "/admin/models/{model_name}/history"),
    ("POST", "/admin/models/{model_name}/deprecation/preview"),
    ("POST", "/admin/models/{model_name}/deprecation/confirm"),
    ("GET", "/admin/models/{model_name}/access"),
    ("PUT", "/admin/models/{model_name}/access/{org_id}"),
    ("DELETE", "/admin/models/{model_name}/access/{org_id}"),
    ("GET", "/admin/organizations/{org_id}/model-access"),
    ("POST", "/admin/services"),
    ("PATCH", "/admin/services/{id}"),
    ("PATCH", "/admin/organizations/{org_id}/limits"),
    ("GET", "/admin/organizations/{org_id}/limits/history"),
    ("GET", "/admin/aml/reports"),
    ("PATCH", "/admin/aml/reports/{report_id}/status"),
    ("GET", "/admin/aml/allowlist"),
    ("POST", "/admin/aml/allowlist"),
    ("DELETE", "/admin/aml/allowlist/{account_id}"),
    ("GET", "/admin/organizations/{org_id}/concurrent-limit"),
    ("PATCH", "/admin/organizations/{org_id}/concurrent-limit"),
    ("GET", "/admin/organizations/{org_id}/priority"),
    ("PATCH", "/admin/organizations/{org_id}/priority"),
    ("GET", "/admin/organizations/{org_id}/fallback"),
    ("PATCH", "/admin/organizations/{org_id}/fallback"),
    ("GET", "/admin/platform/infra-summary"),
    ("GET", "/admin/settings"),
    ("GET", "/admin/settings/{key}"),
    ("PATCH", "/admin/settings/{key}"),
    ("GET", "/admin/invitation-email-deliveries"),
    (
        "POST",
        "/admin/invitation-email-deliveries/{invitation_id}/resend",
    ),
    ("POST", "/admin/users/{user_id}/erasure/preview"),
    ("POST", "/admin/users/{user_id}/erasure"),
    ("POST", "/admin/user-erasures/lookup"),
    ("PUT", "/admin/organizations/{org_id}/members/{user_id}"),
    ("POST", "/admin/access-tokens"),
    ("GET", "/admin/access-tokens"),
    ("DELETE", "/admin/access-tokens/{token_id}"),
    ("POST", "/admin/database-encryption/scan"),
    ("POST", "/admin/database-encryption/jobs"),
    ("GET", "/admin/database-encryption/jobs/{id}"),
    ("GET", "/admin/database-encryption/keys"),
    ("POST", "/admin/database-encryption/jobs/{id}/cancel"),
    ("POST", "/admin/database-encryption/verify"),
    ("GET", "/admin/object-encryption/keys"),
    ("POST", "/admin/object-encryption/scan"),
    ("POST", "/admin/object-encryption/jobs"),
    ("GET", "/admin/object-encryption/jobs/{id}"),
    ("POST", "/admin/object-encryption/jobs/{id}/cancel"),
    ("POST", "/admin/object-encryption/verify"),
    ("GET", "/admin/feature-requests"),
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
    /// JSON lines without span fields, as production writes them. Requests
    /// sent with `oneshot` on this test's current-thread runtime log here.
    fn start() -> (Self, DefaultGuard) {
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

    /// Every line logged so far, emptying the buffer.
    fn take_lines(&self) -> Vec<Value> {
        let mut logs = self
            .0
            .lock()
            .expect("captured logs mutex should not poison");
        String::from_utf8(std::mem::take(&mut *logs))
            .expect("logs are UTF-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("JSON log line"))
            .collect()
    }

    fn take_single_event(&self) -> Map<String, Value> {
        let events: Vec<_> = self
            .take_lines()
            .into_iter()
            .filter(|line| line["fields"]["audit_event"] == "admin_request")
            .collect();
        assert_eq!(events.len(), 1, "one audit event per request: {events:?}");
        assert_eq!(events[0]["level"], "INFO");
        events[0]["fields"]
            .as_object()
            .cloned()
            .expect("fields object")
    }
}

/// A route template with sample values for its parameters.
fn concrete_path(route: &str) -> String {
    let segments: Vec<String> = route
        .split('/')
        .map(|segment| match segment {
            "{model_name}" => "audit-model".to_string(),
            "{account_id}" => "audit.near".to_string(),
            "{key}" => "placement".to_string(),
            parameter if parameter.starts_with('{') => Uuid::new_v4().to_string(),
            literal => literal.to_string(),
        })
        .collect();
    format!("/v1{}", segments.join("/"))
}

/// The `"/admin..."` string literals of a source file: the admin routes it mounts.
fn admin_route_literals(source: &str) -> BTreeSet<&str> {
    source
        .match_indices("\"/admin")
        .map(|(start, _)| {
            let literal = &source[start + 1..];
            &literal[..literal.find('"').expect("closing quote")]
        })
        .collect()
}

fn session_jwt(session_id: Uuid) -> String {
    MockAuthService {
        apikey_repository: Arc::new(database::repositories::ApiKeyRepository::new(
            database::DbPool::uninitialized(),
        )),
    }
    .create_session_access_token(
        UserId(Uuid::parse_str(MOCK_USER_ID).expect("mock user id")),
        Some(SessionId(session_id)),
        test_config().auth.encoding_key,
        1,
    )
    .expect("mock auth signs session tokens")
}

fn authorized(method: Method, uri: &str, token: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", MOCK_USER_AGENT)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("test request")
}

async fn send(router: &Router, request: Request<Body>) -> Response {
    router
        .clone()
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

async fn json_body(response: Response) -> Value {
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads");
    serde_json::from_slice(&body).expect("JSON body")
}

/// Paginated admin lists the page guard must find in the OpenAPI document. A
/// newly documented paginated list is checked without being listed here.
const KNOWN_PAGINATED_ADMIN_LISTS: &[&str] = &[
    "/admin/access-tokens",
    "/admin/aml/allowlist",
    "/admin/aml/reports",
    "/admin/feature-requests",
    "/admin/invitation-email-deliveries",
    "/admin/models",
    "/admin/models/pricing-changes",
    "/admin/models/{model_name}/history",
    "/admin/organizations/{org_id}/limits/history",
];

/// Admin GET routes documented with `limit` and `offset` query parameters.
fn documented_paginated_admin_lists() -> BTreeSet<String> {
    let openapi = serde_json::to_value(api::openapi::ApiDoc::openapi()).expect("OpenAPI JSON");
    openapi["paths"]
        .as_object()
        .expect("OpenAPI paths")
        .iter()
        .filter_map(|(path, operations)| {
            let route = path.strip_prefix("/v1")?;
            let query: BTreeSet<&str> = operations["get"]["parameters"]
                .as_array()?
                .iter()
                .filter(|parameter| parameter["in"] == "query")
                .filter_map(|parameter| parameter["name"].as_str())
                .collect();
            (route.starts_with("/admin") && query.contains("limit") && query.contains("offset"))
                .then(|| route.to_string())
        })
        .collect()
}

/// A paginated list route with the test's fixtures as its parameters.
fn fixture_list_path(route: &str, organization_id: &str, model_name: &str) -> String {
    let segments: Vec<&str> = route
        .split('/')
        .map(|segment| match segment {
            "{org_id}" => organization_id,
            "{model_name}" => model_name,
            parameter if parameter.starts_with('{') => {
                panic!("give {parameter} of {route} a fixture with at least two rows")
            }
            literal => literal,
        })
        .collect();
    format!("/v1{}", segments.join("/"))
}

/// The rows a list response returned: its only top-level array.
fn list_rows(body: &Value, uri: &str) -> usize {
    let arrays: Vec<_> = body
        .as_object()
        .unwrap_or_else(|| panic!("{uri} returns an object"))
        .values()
        .filter_map(Value::as_array)
        .collect();
    assert_eq!(arrays.len(), 1, "{uri} returns one row array: {body}");
    arrays[0].len()
}

#[test]
fn audited_route_table_lists_every_mounted_admin_route() {
    let listed: BTreeSet<&str> = MOUNTED_ADMIN_ROUTES
        .iter()
        .map(|(_, route)| *route)
        .collect();
    let mounted: BTreeSet<&str> = [
        include_str!("../../src/lib.rs"),
        include_str!("../../src/object_encryption.rs"),
    ]
    .into_iter()
    .flat_map(admin_route_literals)
    .collect();
    assert_eq!(
        listed, mounted,
        "list new admin routes in MOUNTED_ADMIN_ROUTES so their audit events are tested"
    );

    let openapi = serde_json::to_value(api::openapi::ApiDoc::openapi()).expect("OpenAPI JSON");
    let documented = openapi["paths"].as_object().expect("OpenAPI paths");
    for (path, operations) in documented {
        let Some(route) = path
            .strip_prefix("/v1")
            .filter(|route| route.starts_with("/admin"))
        else {
            continue;
        };
        for key in operations.as_object().expect("path item").keys() {
            let method = key.to_uppercase();
            if matches!(
                method.as_str(),
                "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
            ) {
                assert!(
                    MOUNTED_ADMIN_ROUTES.contains(&(method.as_str(), route)),
                    "documented admin operation {method} {route} is not in MOUNTED_ADMIN_ROUTES"
                );
            }
        }
    }
}

#[tokio::test]
async fn every_mounted_admin_route_is_audited_once() {
    let (_server, router) = setup_test_server_and_router().await;
    let (logs, _guard) = CapturedLogs::start();

    for (method, route) in MOUNTED_ADMIN_ROUTES {
        logs.take_lines();
        let response = send(
            &router,
            Request::builder()
                .method(*method)
                .uri(concrete_path(route))
                .body(Body::empty())
                .expect("test request"),
        )
        .await;

        // Unauthenticated: admin auth guards the route and denies it.
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {route}"
        );
        // With no client x-request-id, both headers carry the server's id.
        let audit_id = header(&response, AUDIT_HEADER);
        assert_eq!(audit_id, header(&response, REQUEST_ID_HEADER));
        let event = logs.take_single_event();
        assert_eq!(event["request_id"], audit_id.as_str(), "{method} {route}");
        assert_eq!(event["admin_route"], format!("/v1{route}"));
        assert_eq!(event["admin_method"], *method);
        assert_eq!(event["admin_status"], 401);
        assert_eq!(event["admin_auth_outcome"], "denied");
        assert_eq!(event["admin_credential_kind"], "none");
        for absent in [
            "admin_user_id",
            "admin_credential_id",
            "admin_organization_id",
        ] {
            assert!(!event.contains_key(absent), "{method} {route}: {event:?}");
        }
    }
}

#[tokio::test]
async fn admin_token_identity_is_audited_for_allowed_and_denied_requests() {
    let (_server, router, _pool, _mock, database) = setup_test_server_with_pool_and_router().await;
    let tokens = database::repositories::AdminAccessTokenRepository::new(database.pool().clone());
    let creator = Uuid::parse_str(MOCK_USER_ID).expect("mock user id");
    let mut issued = Vec::new();
    for permission in [
        database::models::AdminAccessTokenPermission::ReadOnly,
        database::models::AdminAccessTokenPermission::ReadWrite,
    ] {
        issued.push(
            tokens
                .create(
                    creator,
                    format!("admin-audit-{}", Uuid::new_v4()),
                    "admin audit e2e".to_string(),
                    chrono::Utc::now() + chrono::Duration::hours(1),
                    Some(MOCK_USER_AGENT.to_string()),
                    permission,
                )
                .await
                .expect("token is created"),
        );
    }
    let [(read_only, read_only_token), (read_write, read_write_token)] =
        <[_; 2]>::try_from(issued).unwrap_or_else(|_| panic!("two tokens"));
    let (logs, _guard) = CapturedLogs::start();

    let response = send(
        &router,
        authorized(
            Method::GET,
            "/v1/admin/aml/reports?limit=1",
            &read_only_token,
            json!({}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let returned = json_body(response).await["reports"]
        .as_array()
        .expect("reports")
        .len();
    let event = logs.take_single_event();
    assert_eq!(event["admin_route"], "/v1/admin/aml/reports");
    assert_eq!(event["admin_auth_outcome"], "allowed");
    assert_eq!(event["admin_credential_kind"], "admin_access_token");
    assert_eq!(event["admin_credential_id"], read_only.id.to_string());
    assert_eq!(event["admin_user_id"], MOCK_USER_ID);
    assert_eq!(event["admin_limit"], 1);
    assert_eq!(event["admin_offset"], 0);
    assert_eq!(event["admin_returned_count"], returned);
    assert!(!event.contains_key("admin_organization_id"));

    // Policy denials keep the authenticated identity but name no organization.
    for (method, uri, token, credential_id) in [
        (
            Method::PATCH,
            format!(
                "/v1/admin/organizations/{}/concurrent-limit",
                Uuid::new_v4()
            ),
            &read_only_token,
            read_only.id,
        ),
        (
            Method::GET,
            "/v1/admin/access-tokens".to_string(),
            &read_write_token,
            read_write.id,
        ),
    ] {
        let response = send(
            &router,
            authorized(method, &uri, token, json!({"concurrentLimit": 64})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri}");
        let event = logs.take_single_event();
        assert_eq!(event["admin_status"], 403);
        assert_eq!(event["admin_auth_outcome"], "denied");
        assert_eq!(event["admin_credential_kind"], "admin_access_token");
        assert_eq!(event["admin_credential_id"], credential_id.to_string());
        assert_eq!(event["admin_user_id"], MOCK_USER_ID);
        assert!(!event.contains_key("admin_organization_id"));
    }

    // A rejected credential identifies no one.
    assert!(tokens
        .revoke(read_write.id, creator, "admin audit e2e".to_string())
        .await
        .expect("token is revoked"));
    let response = send(
        &router,
        authorized(
            Method::GET,
            "/v1/admin/aml/reports",
            &read_write_token,
            json!({}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let event = logs.take_single_event();
    assert_eq!(event["admin_auth_outcome"], "denied");
    assert_eq!(event["admin_credential_kind"], "admin_access_token");
    assert!(!event.contains_key("admin_credential_id"));
    assert!(!event.contains_key("admin_user_id"));
}

#[tokio::test]
async fn list_handlers_record_validated_pages_and_organization_scope() {
    let (server, router) = setup_test_server_and_router().await;
    let organization_id = create_org(&server).await.id;
    let session_id = Uuid::new_v4();
    let session = session_jwt(session_id);
    let (logs, _guard) = CapturedLogs::start();

    let lists = [
        (
            "/v1/admin/aml/reports?limit=1".to_string(),
            "reports",
            1,
            0,
            None,
        ),
        (
            "/v1/admin/aml/allowlist?limit=1&offset=2".to_string(),
            "accounts",
            1,
            2,
            None,
        ),
        (
            format!(
                "/v1/admin/invitation-email-deliveries?organization_id={organization_id}&limit=1"
            ),
            "deliveries",
            1,
            0,
            Some(&organization_id),
        ),
    ];
    for (uri, rows_field, limit, offset, organization) in lists {
        let response = send(&router, authorized(Method::GET, &uri, &session, json!({}))).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        let rows = json_body(response).await[rows_field]
            .as_array()
            .unwrap_or_else(|| panic!("{uri} lists {rows_field}"))
            .len();
        let event = logs.take_single_event();
        assert_eq!(event["admin_auth_outcome"], "allowed", "{uri}");
        assert_eq!(event["admin_credential_kind"], "session");
        assert_eq!(event["admin_credential_id"], session_id.to_string());
        assert_eq!(event["admin_user_id"], MOCK_USER_ID);
        assert_eq!(event["admin_limit"], limit, "{uri}");
        assert_eq!(event["admin_offset"], offset, "{uri}");
        assert_eq!(event["admin_returned_count"], rows, "{uri}");
        match organization {
            Some(organization_id) => {
                assert_eq!(event["admin_organization_id"], organization_id.as_str())
            }
            None => assert!(!event.contains_key("admin_organization_id"), "{uri}"),
        }
    }

    // A page the handler rejects is not recorded.
    let response = send(
        &router,
        authorized(
            Method::GET,
            "/v1/admin/aml/reports?limit=0",
            &session,
            json!({}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let event = logs.take_single_event();
    assert_eq!(event["admin_status"], 400);
    for absent in ["admin_limit", "admin_offset", "admin_returned_count"] {
        assert!(!event.contains_key(absent), "{event:?}");
    }

    // An allowed org-scoped write names its organization.
    let response = send(
        &router,
        authorized(
            Method::PATCH,
            &format!("/v1/admin/organizations/{organization_id}/concurrent-limit"),
            &session,
            json!({"concurrentLimit": 64}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let event = logs.take_single_event();
    assert_eq!(
        event["admin_route"],
        "/v1/admin/organizations/{org_id}/concurrent-limit"
    );
    assert_eq!(event["admin_method"], "PATCH");
    assert_eq!(event["admin_auth_outcome"], "allowed");
    assert_eq!(event["admin_organization_id"], organization_id.as_str());
}

#[tokio::test]
async fn audit_events_exclude_request_content_on_the_real_router() {
    const EMAIL: &str = "audit-e2e-sentinel@customer.example";
    const BODY: &str = "AUDIT_E2E_BODY_SENTINEL";
    const USER_AGENT_VALUE: &str = "AUDIT_E2E_USER_AGENT_SENTINEL";
    const FORWARDED_IP: &str = "198.51.100.23";
    let (_server, router) = setup_test_server_and_router().await;
    let session = get_session_id();
    let client_request_id = Uuid::new_v4().to_string();
    let (logs, _guard) = CapturedLogs::start();

    for uri in [
        format!("/v1/admin/models?search={EMAIL}&limit=1"),
        format!("/v1/admin/users/{EMAIL}/lookup"),
    ] {
        let response = send(
            &router,
            Request::builder()
                .method(Method::GET)
                .uri(&uri)
                .header("Authorization", format!("Bearer {session}"))
                .header("User-Agent", USER_AGENT_VALUE)
                .header("X-Forwarded-For", FORWARDED_IP)
                .header(REQUEST_ID_HEADER, &client_request_id)
                .header(AUDIT_HEADER, &client_request_id)
                .body(Body::from(BODY))
                .expect("test request"),
        )
        .await;
        assert_ne!(header(&response, AUDIT_HEADER), client_request_id);

        let lines = logs.take_lines();
        let audit_events: Vec<_> = lines
            .iter()
            .filter(|line| line["fields"]["audit_event"] == "admin_request")
            .collect();
        assert_eq!(audit_events.len(), 1, "{uri}");
        let production_lines = lines
            .iter()
            .filter(|line| !matches!(line["level"].as_str(), Some("DEBUG" | "TRACE")));
        for line in production_lines.chain(audit_events.iter().copied()) {
            let line = line.to_string();
            for forbidden in [
                EMAIL,
                BODY,
                USER_AGENT_VALUE,
                FORWARDED_IP,
                session.as_str(),
                client_request_id.as_str(),
            ] {
                assert!(!line.contains(forbidden), "{forbidden} leaked: {line}");
            }
        }
    }
}

#[test]
fn page_guard_finds_every_known_paginated_admin_list() {
    let documented = documented_paginated_admin_lists();
    for route in KNOWN_PAGINATED_ADMIN_LISTS {
        assert!(
            documented.contains(*route),
            "{route} must document its limit and offset query parameters"
        );
    }
}

/// Guard: every documented paginated admin list records the page it served,
/// so a new list handler cannot leave its audit event without one.
#[tokio::test]
async fn every_documented_paginated_admin_list_records_its_page() {
    let (server, router) = setup_test_server_and_router().await;
    // Fixtures so both pages below can return rows: limits history fails on
    // an empty page, and model history fails for a model without history.
    let organization_id = setup_org_with_credits(&server, 1_000_000_000).await.id;
    add_credits_with_type(
        &server,
        &organization_id,
        "grant",
        None,
        1_000_000_000,
        "USD",
        &get_session_id(),
    )
    .await;
    let model_name = format!("admin-audit-history-{}", Uuid::new_v4());
    for input_cost in [1_000, 2_000] {
        let model = serde_json::from_value(json!({
            "inputCostPerToken": {"amount": input_cost, "currency": "USD"},
            "outputCostPerToken": {"amount": 2_000, "currency": "USD"},
            "modelDisplayName": "Admin audit history fixture",
            "modelDescription": "Admin audit history fixture",
            "contextLength": 4096,
            "verifiable": true,
            "isActive": false,
            "changeReason": "admin audit e2e"
        }))
        .expect("model fixture");
        seed_models(
            &server,
            std::collections::HashMap::from([(model_name.clone(), model)]),
            get_session_id(),
        )
        .await;
    }
    let feature_request = server
        .post("/v1/feature-requests")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "kind": "model",
            "key": format!("admin-audit-{}", Uuid::new_v4()),
            "title": "Admin audit fixture"
        }))
        .await;
    assert_eq!(feature_request.status_code(), StatusCode::OK);
    let access_token = server
        .post("/v1/admin/access-tokens")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "name": format!("admin-audit-{}", Uuid::new_v4()),
            "reason": "admin audit e2e",
            "expires_in_hours": 1
        }))
        .await;
    assert_eq!(access_token.status_code(), StatusCode::OK);
    let session = session_jwt(Uuid::new_v4());
    let (logs, _guard) = CapturedLogs::start();

    for route in documented_paginated_admin_lists() {
        let path = fixture_list_path(&route, &organization_id, &model_name);
        for (limit, offset) in [(2, 0), (1, 1)] {
            let uri = format!("{path}?limit={limit}&offset={offset}");
            let response = send(&router, authorized(Method::GET, &uri, &session, json!({}))).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            let rows = list_rows(&json_body(response).await, &uri);
            let event = logs.take_single_event();
            assert_eq!(event["admin_route"], format!("/v1{route}"), "{uri}");
            assert_eq!(
                (
                    event.get("admin_limit"),
                    event.get("admin_offset"),
                    event.get("admin_returned_count")
                ),
                (
                    Some(&json!(limit)),
                    Some(&json!(offset)),
                    Some(&json!(rows))
                ),
                "{uri} must record the page it served"
            );
            if route.contains("{org_id}") {
                assert_eq!(event["admin_organization_id"], organization_id.as_str());
            }
        }
    }
}

#[tokio::test]
async fn paginated_lists_record_no_page_when_the_request_fails() {
    let (_server, router) = setup_test_server_and_router().await;
    let session = session_jwt(Uuid::new_v4());
    let (logs, _guard) = CapturedLogs::start();

    for (uri, status) in [
        (
            "/v1/admin/models?limit=0".to_string(),
            StatusCode::BAD_REQUEST,
        ),
        (
            format!(
                "/v1/admin/models/admin-audit-missing-{}/history",
                Uuid::new_v4()
            ),
            StatusCode::NOT_FOUND,
        ),
        (
            format!("/v1/admin/organizations/{}/limits/history", Uuid::new_v4()),
            StatusCode::NOT_FOUND,
        ),
        // The pricing-change service, not the handler, validates this page.
        (
            "/v1/admin/models/pricing-changes?limit=0".to_string(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/v1/admin/access-tokens?offset=-1".to_string(),
            StatusCode::BAD_REQUEST,
        ),
        // A valid page with an invalid filter.
        (
            "/v1/admin/feature-requests?kind=unknown".to_string(),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = send(&router, authorized(Method::GET, &uri, &session, json!({}))).await;
        assert_eq!(response.status(), status, "{uri}");
        let event = logs.take_single_event();
        assert_eq!(event["admin_auth_outcome"], "allowed", "{uri}");
        assert_eq!(event["admin_status"], status.as_u16(), "{uri}");
        for absent in ["admin_limit", "admin_offset", "admin_returned_count"] {
            assert!(!event.contains_key(absent), "{uri}: {event:?}");
        }
    }
}
