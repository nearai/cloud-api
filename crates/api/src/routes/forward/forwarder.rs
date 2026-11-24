//! Streaming forwarder to cloud-platform's edge door.
//!
//! Requests are rebuilt from an allow-list of headers; responses are passed
//! back through an allow-list too. Logging is limited to the route template,
//! method, status, error kind and duration: never the path, query, headers,
//! body, secret or the HTTP client's error text.

use std::time::{Duration, Instant};

use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderName, HeaderValue, Request, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures_util::StreamExt;
use http_body_util::{BodyExt, Limited};

use super::{
    config::ForwarderConfig,
    table::{unsafe_segment, Entry},
};
use crate::middleware::request_correlation::{RequestCorrelation, REQUEST_ID_HEADER};
use crate::models::ErrorResponse;

/// Cloud-platform refuses larger bodies with 413; reject them here first.
pub const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;

pub const FORWARDER_HEADER: &str = "x-nearai-forwarder";

const REQUEST_HEADERS: [HeaderName; 4] = [
    header::AUTHORIZATION,
    header::USER_AGENT,
    header::CONTENT_TYPE,
    header::ACCEPT,
];

/// Response headers that reach the caller. Everything else is dropped:
/// `Access-Control-*` (cloud-api's own CORS layer answers), `Set-Cookie` and
/// hop-by-hop headers.
const RESPONSE_HEADERS: [&str; 9] = [
    "content-type",
    "content-encoding",
    "content-length",
    "location",
    "retry-after",
    "x-contract-version",
    "cache-control",
    "etag",
    "vary",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    TooLarge,
    BadRequestBody,
    Connect,
    Timeout,
    Upstream,
    Body,
    Path,
    BodyTimeout,
}

impl FailureKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::TooLarge => "too_large",
            Self::BadRequestBody => "request_body",
            Self::Connect => "connect",
            Self::Timeout => "timeout",
            Self::Upstream => "upstream",
            Self::Body => "body",
            Self::Path => "path",
            Self::BodyTimeout => "body_timeout",
        }
    }
}

pub struct Forwarder {
    client: reqwest::Client,
    base_url: String,
    /// Path component of `base_url` without a trailing slash.
    base_path: String,
    body_read_timeout: Duration,
    secret: HeaderValue,
}

impl std::fmt::Debug for Forwarder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Forwarder")
            .field("base_url", &self.base_url)
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl Forwarder {
    pub fn new(config: &ForwarderConfig) -> anyhow::Result<Self> {
        let mut secret = HeaderValue::from_str(&config.secret)
            .map_err(|_| anyhow::anyhow!("forwarder secret is not a valid header value"))?;
        secret.set_sensitive(true);
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.total_timeout)
            .redirect(reqwest::redirect::Policy::none())
            // Never route the secret through an environment-configured proxy.
            .no_proxy()
            .build()
            .map_err(|_| anyhow::anyhow!("forwarder HTTP client could not be built"))?;
        let base_url = config.base_url.trim_end_matches('/').to_string();
        let base_path = reqwest::Url::parse(&base_url)
            .map_err(|_| anyhow::anyhow!("forwarder base URL is not valid"))?
            .path()
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            client,
            base_url,
            base_path,
            body_read_timeout: config.body_read_timeout,
            secret,
        })
    }

    pub async fn forward(&self, entry: &Entry, req: Request<Body>) -> Response {
        let started = Instant::now();
        let (parts, body) = req.into_parts();

        let mut url = format!("{}{}", self.base_url, parts.uri.path());
        if let Some(query) = parts.uri.query() {
            url.push('?');
            url.push_str(query);
        }
        // The URL parser collapses dot segments; whatever it would send must
        // be exactly the base path plus the path the caller asked for.
        let url = match reqwest::Url::parse(&url) {
            Ok(u)
                if !parts.uri.path().split('/').any(unsafe_segment)
                    && u.path() == format!("{}{}", self.base_path, parts.uri.path()) =>
            {
                u
            }
            _ => return fail(entry, FailureKind::Path, started),
        };

        let collected = tokio::time::timeout(
            self.body_read_timeout,
            Limited::new(body, MAX_REQUEST_BODY_BYTES).collect(),
        )
        .await;
        let body = match collected {
            Err(_) => return fail(entry, FailureKind::BodyTimeout, started),
            Ok(Ok(collected)) => collected.to_bytes(),
            Ok(Err(e)) => {
                let kind = if e
                    .downcast_ref::<http_body_util::LengthLimitError>()
                    .is_some()
                {
                    FailureKind::TooLarge
                } else {
                    FailureKind::BadRequestBody
                };
                return fail(entry, kind, started);
            }
        };

        let mut headers = HeaderMap::new();
        for name in &REQUEST_HEADERS {
            for value in parts.headers.get_all(name) {
                headers.append(name.clone(), value.clone());
            }
        }
        for value in parts.headers.get_all(header::ACCEPT_ENCODING) {
            headers.append(header::ACCEPT_ENCODING, value.clone());
        }
        headers.insert(
            HeaderName::from_static(FORWARDER_HEADER),
            self.secret.clone(),
        );
        if let Some(correlation) = parts.extensions.get::<RequestCorrelation>() {
            if let Ok(value) = HeaderValue::from_str(&correlation.request_id.to_string()) {
                headers.insert(HeaderName::from_static(REQUEST_ID_HEADER), value);
            }
        }

        let upstream = match self
            .client
            .request(parts.method, url)
            .headers(headers)
            .body(body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => {
                // Classified only; the error text can carry the URL.
                let kind = if e.is_timeout() {
                    FailureKind::Timeout
                } else if e.is_connect() {
                    FailureKind::Connect
                } else {
                    FailureKind::Upstream
                };
                return fail(entry, kind, started);
            }
        };

        let status = upstream.status();
        let mut response_headers = HeaderMap::new();
        for name in RESPONSE_HEADERS {
            for value in upstream.headers().get_all(name) {
                response_headers.append(HeaderName::from_static(name), value.clone());
            }
        }

        let template = entry.template;
        let method = entry.method.clone();
        let stream = upstream.bytes_stream().scan(false, move |ended, item| {
            let out = if *ended {
                None
            } else {
                match item {
                    Ok(bytes) => Some(Ok::<_, std::convert::Infallible>(bytes)),
                    Err(e) => {
                        // A body cut mid-stream ends the stream.
                        *ended = true;
                        let kind = if e.is_timeout() {
                            FailureKind::Timeout
                        } else {
                            FailureKind::Body
                        };
                        tracing::warn!(
                            template,
                            method = %method,
                            status = status.as_u16(),
                            kind = kind.as_str(),
                            duration_ms = started.elapsed().as_millis() as u64,
                            "forwarded response body ended early"
                        );
                        None
                    }
                }
            };
            futures_util::future::ready(out)
        });

        tracing::debug!(
            template = entry.template,
            method = %entry.method,
            status = status.as_u16(),
            duration_ms = started.elapsed().as_millis() as u64,
            "forwarded"
        );

        let mut response = Response::new(Body::from_stream(stream));
        *response.status_mut() = status;
        *response.headers_mut() = response_headers;
        response
    }
}

fn fail(entry: &Entry, kind: FailureKind, started: Instant) -> Response {
    let (status, error_type, code, message) = match kind {
        FailureKind::BodyTimeout => (
            StatusCode::REQUEST_TIMEOUT,
            "invalid_request_error",
            "request_timeout",
            "Request body was not received in time.",
        ),
        FailureKind::Path => (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_request_path",
            "Request path is not allowed.",
        ),
        FailureKind::TooLarge => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "request_too_large",
            "Request body is too large.",
        ),
        FailureKind::BadRequestBody => (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_request_body",
            "Request body could not be read.",
        ),
        FailureKind::Timeout => (
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_error",
            "upstream_timeout",
            "The upstream service timed out.",
        ),
        FailureKind::Connect | FailureKind::Upstream | FailureKind::Body => (
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "upstream_unavailable",
            "The upstream service is unavailable.",
        ),
    };
    tracing::warn!(
        template = entry.template,
        method = %entry.method,
        status = status.as_u16(),
        kind = kind.as_str(),
        duration_ms = started.elapsed().as_millis() as u64,
        "forward failed"
    );
    let mut body = ErrorResponse::new(message.to_string(), error_type.to_string());
    body.error.code = Some(code.to_string());
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SECRET: &str = "edge-secret";

    fn entry() -> Entry {
        Entry::new(Method::POST, "/v1/things/{id}")
    }

    fn config(base_url: &str) -> ForwarderConfig {
        ForwarderConfig {
            base_url: base_url.to_string(),
            secret: SECRET.to_string(),
            connect_timeout: Duration::from_millis(500),
            total_timeout: Duration::from_secs(5),
            body_read_timeout: Duration::from_secs(5),
        }
    }

    fn forwarder(server: &MockServer) -> Forwarder {
        Forwarder::new(&config(&server.uri())).unwrap()
    }

    async fn ok_mock(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/v1/things/42"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn request_headers_are_allow_listed() {
        let server = MockServer::start().await;
        ok_mock(&server).await;
        let correlation = RequestCorrelation {
            request_id: uuid::Uuid::new_v4(),
        };
        let mut req = Request::builder()
            .method(Method::POST)
            .uri("/v1/things/42?a=1&b=two")
            .header("authorization", "Bearer one")
            .header("authorization", "Bearer two")
            .header(
                "user-agent",
                HeaderValue::from_bytes(b"Agent/1.0  caf\xe9 (x)").unwrap(),
            )
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .header("accept-encoding", "gzip")
            .header("cookie", "sid=1")
            .header("origin", "https://evil.example")
            .header("x-forwarded-for", "1.2.3.4")
            .header("x-nearai-forwarder", "spoofed")
            .header("x-request-id", "client-id")
            .body(Body::from("{\"a\":1}"))
            .unwrap();
        req.extensions_mut().insert(correlation);

        let response = forwarder(&server).forward(&entry(), req).await;
        assert_eq!(response.status(), 200);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"ok");

        let received = &server.received_requests().await.unwrap()[0];
        let auth: Vec<_> = received.headers.get_all("authorization").iter().collect();
        assert_eq!(auth.len(), 2);
        assert_eq!(auth[0], "Bearer one");
        assert_eq!(auth[1], "Bearer two");
        assert_eq!(
            received.headers.get("user-agent").unwrap().as_bytes(),
            b"Agent/1.0  caf\xe9 (x)"
        );
        assert_eq!(
            received.headers.get("content-type").unwrap(),
            "application/json"
        );
        assert_eq!(received.headers.get("accept").unwrap(), "application/json");
        assert_eq!(received.headers.get("accept-encoding").unwrap(), "gzip");
        assert_eq!(received.url.query(), Some("a=1&b=two"));
        assert_eq!(&received.body[..], b"{\"a\":1}");
        for name in ["cookie", "origin", "x-forwarded-for"] {
            assert!(received.headers.get(name).is_none(), "{name} leaked");
        }
        let forwarder_values: Vec<_> = received
            .headers
            .get_all("x-nearai-forwarder")
            .iter()
            .collect();
        assert_eq!(forwarder_values.len(), 1);
        assert_eq!(forwarder_values[0], SECRET);
        let ids: Vec<_> = received.headers.get_all("x-request-id").iter().collect();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], correlation.request_id.to_string().as_str());
    }

    #[tokio::test]
    async fn content_encoding_and_compressed_bytes_pass_through() {
        // Gzip magic plus arbitrary bytes: only byte-exactness matters.
        let compressed: Vec<u8> = vec![0x1f, 0x8b, 0x08, 0x00, 0xde, 0xad, 0xbe, 0xef, 0x00, 0xff];
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/things/42"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-encoding", "gzip")
                    .set_body_bytes(compressed.clone()),
            )
            .mount(&server)
            .await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/v1/things/42")
            .header("accept-encoding", "gzip")
            .body(Body::empty())
            .unwrap();
        let response = forwarder(&server).forward(&entry(), req).await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers().get("content-encoding").unwrap(), "gzip");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], &compressed[..]);
    }

    #[tokio::test]
    async fn no_user_agent_is_added_and_no_request_id_without_extension() {
        let server = MockServer::start().await;
        ok_mock(&server).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/v1/things/42")
            .header("x-request-id", "client-id")
            .body(Body::empty())
            .unwrap();
        forwarder(&server).forward(&entry(), req).await;
        let received = &server.received_requests().await.unwrap()[0];
        assert!(received.headers.get("user-agent").is_none());
        assert!(received.headers.get("x-request-id").is_none());
        assert_eq!(received.headers.get("x-nearai-forwarder").unwrap(), SECRET);
    }

    #[tokio::test]
    async fn redirect_is_passed_not_followed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/things/42"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", "https://idp.example/auth?x=1"),
            )
            .mount(&server)
            .await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/v1/things/42")
            .body(Body::empty())
            .unwrap();
        let response = forwarder(&server).forward(&entry(), req).await;
        assert_eq!(response.status(), 302);
        assert_eq!(
            response.headers().get("location").unwrap(),
            "https://idp.example/auth?x=1"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn body_limit_is_one_mib() {
        let server = MockServer::start().await;
        ok_mock(&server).await;
        let f = forwarder(&server);
        let at_limit = Request::builder()
            .method(Method::POST)
            .uri("/v1/things/42")
            .body(Body::from(vec![b'a'; MAX_REQUEST_BODY_BYTES]))
            .unwrap();
        assert_eq!(f.forward(&entry(), at_limit).await.status(), 200);

        let over = Request::builder()
            .method(Method::POST)
            .uri("/v1/things/42")
            .body(Body::from(vec![b'a'; MAX_REQUEST_BODY_BYTES + 1]))
            .unwrap();
        let response = f.forward(&entry(), over).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json["error"]["message"].is_string());
        assert!(json["error"]["type"].is_string());
        assert!(json["error"]["code"].is_string());
        assert!(json["error"].get("param").is_some());
        // Only the at-limit request reached the upstream.
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn response_headers_are_allow_listed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/things/42"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("content-type", "application/json")
                    .insert_header("retry-after", "7")
                    .insert_header("x-contract-version", "3")
                    .insert_header("cache-control", "no-store")
                    .insert_header("etag", "\"v1\"")
                    .insert_header("vary", "Origin")
                    .insert_header("access-control-allow-origin", "*")
                    .insert_header("access-control-expose-headers", "x-request-id")
                    .insert_header("set-cookie", "sid=1")
                    .insert_header("x-request-id", "cp-id")
                    .set_body_string("{}"),
            )
            .mount(&server)
            .await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/v1/things/42")
            .body(Body::empty())
            .unwrap();
        let response = forwarder(&server).forward(&entry(), req).await;
        assert_eq!(response.status(), 429);
        let headers = response.headers();
        assert_eq!(headers.get("retry-after").unwrap(), "7");
        assert_eq!(headers.get("x-contract-version").unwrap(), "3");
        assert_eq!(headers.get("cache-control").unwrap(), "no-store");
        assert_eq!(headers.get("etag").unwrap(), "\"v1\"");
        assert_eq!(headers.get("vary").unwrap(), "Origin");
        assert!(headers.get("content-type").is_some());
        assert!(headers.get("content-length").is_some());
        for name in [
            "access-control-allow-origin",
            "access-control-expose-headers",
            "set-cookie",
            "x-request-id",
        ] {
            assert!(headers.get(name).is_none(), "{name} leaked");
        }
    }

    #[tokio::test]
    async fn slow_upstream_is_504() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)))
            .mount(&server)
            .await;
        let mut cfg = config(&server.uri());
        cfg.total_timeout = Duration::from_millis(200);
        let req = Request::builder()
            .method(Method::POST)
            .uri("/v1/things/42")
            .body(Body::empty())
            .unwrap();
        let response = Forwarder::new(&cfg).unwrap().forward(&entry(), req).await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[tokio::test]
    async fn refused_connection_is_502() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let req = Request::builder()
            .method(Method::POST)
            .uri("/v1/things/42")
            .body(Body::empty())
            .unwrap();
        let response = Forwarder::new(&config(&format!("http://{addr}")))
            .unwrap()
            .forward(&entry(), req)
            .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        // Fixed generic message: no address.
        assert!(!String::from_utf8_lossy(&bytes).contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn body_cut_mid_stream_ends_the_stream() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n0123456789")
                .await
                .unwrap();
            // Dropping the socket cuts the body short.
        });
        let req = Request::builder()
            .method(Method::POST)
            .uri("/v1/things/42")
            .body(Body::empty())
            .unwrap();
        let response = Forwarder::new(&config(&format!("http://{addr}")))
            .unwrap()
            .forward(&entry(), req)
            .await;
        assert_eq!(response.status(), 200);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"0123456789");
    }

    #[tokio::test]
    async fn unsafe_paths_are_refused_without_reaching_upstream() {
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let f = forwarder(&server);
        let table = super::super::table::ForwardTable::new(vec![Entry::new(
            Method::POST,
            "/v1/things/{id}",
        )]);
        for bad in [
            "/v1/things/..",
            "/v1/things/.",
            "/v1/things/%2e%2e",
            "/v1/things/%2E%2e",
            "/v1/things/a%2Fb",
            "/v1/things/a%5Cb",
            "/v1/things/a\\b",
            "/v1/things/x/../../../admin",
        ] {
            let Ok(uri) = bad.parse::<axum::http::Uri>() else {
                continue; // not even a valid request target
            };
            assert!(table.r#match(&Method::POST, uri.path()).is_none(), "{bad}");
            let req = Request::builder()
                .method(Method::POST)
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            let response = f.forward(&entry(), req).await;
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["error"]["code"], "invalid_request_path");
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn stalled_request_body_is_408() {
        let server = MockServer::start().await;
        ok_mock(&server).await;
        let mut cfg = config(&server.uri());
        cfg.body_read_timeout = Duration::from_millis(100);
        let stalled = futures_util::stream::pending::<Result<bytes::Bytes, std::io::Error>>();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/v1/things/42")
            .body(Body::from_stream(stalled))
            .unwrap();
        let response = Forwarder::new(&cfg).unwrap().forward(&entry(), req).await;
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["code"], "request_timeout");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[test]
    fn debug_redacts_the_secret() {
        let f = Forwarder::new(&config("http://127.0.0.1:9")).unwrap();
        assert!(!format!("{f:?}").contains(SECRET));
    }
}
