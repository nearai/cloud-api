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
use std::collections::BTreeSet;
use std::io;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use tracing::subscriber::DefaultGuard;
use utoipa::OpenApi;
use uuid::Uuid;

const AUDIT_HEADER: &str = "x-admin-audit-request-id";
const REQUEST_ID_HEADER: &str = "x-request-id";

/// Every admin route the app mounts: `build_admin_routes`, the database-encryption routes (mounted because
/// the e2e configuration has a valid encryption key), and the object-encryption
/// router (using mock storage).
const MOUNTED_ADMIN_ROUTES: &[(&str, &str)] = &[
    ("GET", "/admin/platform/infra-summary"),
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
            "/v1/admin/database-encryption/keys",
            &read_only_token,
            json!({}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let event = logs.take_single_event();
    assert_eq!(event["admin_route"], "/v1/admin/database-encryption/keys");
    assert_eq!(event["admin_auth_outcome"], "allowed");
    assert_eq!(event["admin_credential_kind"], "admin_access_token");
    assert_eq!(event["admin_credential_id"], read_only.id.to_string());
    assert_eq!(event["admin_user_id"], MOCK_USER_ID);
    assert!(!event.contains_key("admin_organization_id"));

    // Policy denials keep the authenticated identity but name no organization.
    for (method, uri, token, credential_id) in [
        (
            Method::POST,
            "/v1/admin/database-encryption/jobs".to_string(),
            &read_only_token,
            read_only.id,
        ),
        (
            Method::POST,
            "/v1/admin/object-encryption/jobs".to_string(),
            &read_only_token,
            read_only.id,
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
            "/v1/admin/database-encryption/keys",
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
        format!("/v1/admin/database-encryption/keys?recipient_email={EMAIL}&limit=1"),
        format!("/v1/admin/platform/infra-summary?lookup={EMAIL}"),
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
