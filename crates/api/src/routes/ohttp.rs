use std::io::Cursor;
use std::time::Instant;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{AsyncReadExt, AsyncWriteExt};
use http_body_util::BodyExt;
use tokio_util::compat::TokioAsyncWriteCompatExt;
use tracing::{info, warn};

use crate::models::ErrorResponse;
use crate::routes::api::AppState;

/// Simple error type local to this module — maps to HTTP status + OpenAI error envelope.
pub enum OhttpError {
    NotEnabled,
    BadRequest(String),
    Internal(String),
}

impl IntoResponse for OhttpError {
    fn into_response(self) -> Response {
        let (status, msg, kind) = match self {
            OhttpError::NotEnabled => (
                StatusCode::NOT_FOUND,
                "OHTTP not enabled".to_string(),
                "not_found_error",
            ),
            OhttpError::BadRequest(m) => (StatusCode::BAD_REQUEST, m, "invalid_request_error"),
            OhttpError::Internal(m) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                m,
                "internal_server_error",
            ),
        };
        (
            status,
            axum::response::Json(ErrorResponse::new(msg, kind.to_string())),
        )
            .into_response()
    }
}

/// `GET /.well-known/ohttp-gateway` and `GET /v1/ohttp/config`
///
/// Returns the OHTTP key configuration (HPKE public key + ciphersuites)
/// in the RFC 9458 wire format (`application/ohttp-keys`).
pub async fn ohttp_config(State(state): State<AppState>) -> Result<Response, OhttpError> {
    let gateway = state.ohttp_gateway.as_ref().ok_or(OhttpError::NotEnabled)?;

    Ok((
        StatusCode::OK,
        [("content-type", "application/ohttp-keys")],
        gateway.config_bytes().to_vec(),
    )
        .into_response())
}

/// `POST /ohttp`
///
/// Accepts OHTTP-encapsulated requests and returns encapsulated responses.
///
/// Dispatches based on Content-Type:
/// - `message/ohttp-req` → standard OHTTP (full request/response)
/// - `message/ohttp-chunked-req` → chunked OHTTP (streaming SSE)
///
/// **Authorization:** An outer `Authorization: Bearer sk-…` on the OHTTP POST
/// is injected into the inner loopback request, overriding any `Authorization`
/// inside the BHTTP payload. This lets a relay hold the API key while clients
/// only see encrypted messages. Inner `Authorization` is used when no outer
/// Bearer is present (backward-compatible).
///
/// Auth, rate-limiting, and usage accounting are applied by the normal
/// completion middleware stack on the loopback request.
pub async fn ohttp_relay(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Result<Response, OhttpError> {
    let gateway = state.ohttp_gateway.as_ref().ok_or(OhttpError::NotEnabled)?;

    let outer_authorization = request.headers().get(header::AUTHORIZATION).cloned();

    let chunked = request
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("ohttp-chunked"));

    let enc_request = request
        .into_body()
        .collect()
        .await
        .map_err(|e| OhttpError::BadRequest(format!("Failed to read request body: {e}")))?
        .to_bytes();

    if enc_request.is_empty() {
        return Err(OhttpError::BadRequest("Empty OHTTP request".to_string()));
    }

    if chunked {
        ohttp_relay_chunked(&state, gateway, &enc_request, outer_authorization).await
    } else {
        ohttp_relay_standard(&state, gateway, &enc_request, outer_authorization).await
    }
}

/// Standard OHTTP: decapsulate full request, forward via loopback, encapsulate full response.
async fn ohttp_relay_standard(
    state: &AppState,
    gateway: &crate::ohttp_gateway::OhttpGateway,
    enc_request: &[u8],
    outer_authorization: Option<HeaderValue>,
) -> Result<Response, OhttpError> {
    let start = Instant::now();

    let (bhttp_request, server_response) = gateway.decapsulate(enc_request).map_err(|e| {
        warn!(error = %e, "OHTTP decapsulation failed");
        OhttpError::BadRequest(format!("OHTTP decapsulation failed: {e}"))
    })?;

    let decap_ms = start.elapsed().as_millis();

    let (request_builder, path_str) =
        parse_bhttp_and_build_loopback(state, &bhttp_request, outer_authorization.as_ref())?;

    let loopback_response = send_loopback(request_builder).await?;

    let response_status = loopback_response.status().as_u16();
    let bhttp_status =
        bhttp::StatusCode::try_from(response_status).unwrap_or(bhttp::StatusCode::OK);
    let mut bhttp_response = bhttp::Message::response(bhttp_status);
    copy_response_headers(&loopback_response, &mut bhttp_response);

    let response_body = loopback_response.bytes().await.map_err(|e| {
        warn!(error = %e, "Failed to read loopback response body");
        OhttpError::Internal(e.to_string())
    })?;
    bhttp_response.write_content(&response_body);

    let mut bhttp_bytes = Vec::new();
    bhttp_response
        .write_bhttp(bhttp::Mode::KnownLength, &mut bhttp_bytes)
        .map_err(|e| OhttpError::Internal(format!("Binary HTTP encoding failed: {e}")))?;

    let enc_response = server_response
        .encapsulate(&bhttp_bytes)
        .map_err(|e| OhttpError::Internal(format!("OHTTP encapsulation failed: {e}")))?;

    info!(
        decap_ms,
        total_ms = start.elapsed().as_millis(),
        inner_status = response_status,
        inner_path = path_str,
        "OHTTP standard request processed"
    );

    Ok((
        StatusCode::OK,
        [(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("message/ohttp-res"),
        )],
        enc_response,
    )
        .into_response())
}

/// Chunked OHTTP: decapsulate request, stream encrypted response chunks.
async fn ohttp_relay_chunked(
    state: &AppState,
    gateway: &crate::ohttp_gateway::OhttpGateway,
    enc_request: &[u8],
    outer_authorization: Option<HeaderValue>,
) -> Result<Response, OhttpError> {
    use futures_util::StreamExt;

    let start = Instant::now();

    let server = gateway.clone_server();
    let mut server_request = server.decapsulate_stream(enc_request);

    let mut bhttp_request = Vec::new();
    server_request
        .read_to_end(&mut bhttp_request)
        .await
        .map_err(|e| {
            warn!(error = %e, "Chunked OHTTP decapsulation failed");
            OhttpError::BadRequest(format!("Chunked OHTTP decapsulation failed: {e}"))
        })?;

    let decap_ms = start.elapsed().as_millis();

    let (request_builder, path_str) =
        parse_bhttp_and_build_loopback(state, &bhttp_request, outer_authorization.as_ref())?;

    let loopback_response = send_loopback(request_builder).await?;
    let response_status = loopback_response.status().as_u16();
    let response_headers = collect_response_headers(&loopback_response);

    // Pipe: write side → OHTTP encrypt, read side → HTTP response body.
    let (read_half, write_half) = tokio::io::duplex(64 * 1024);

    let mut ohttp_writer = server_request
        .response(write_half.compat_write())
        .map_err(|e| {
            warn!(error = %e, "Failed to create chunked OHTTP response writer");
            OhttpError::Internal(format!("OHTTP stream setup failed: {e}"))
        })?;

    info!(
        decap_ms,
        inner_status = response_status,
        inner_path = path_str,
        "Chunked OHTTP request processed"
    );

    tokio::spawn(async move {
        if let Err(e) = write_indeterminate_response_header(
            &mut ohttp_writer,
            response_status,
            &response_headers,
        )
        .await
        {
            warn!(error = %e, "Failed to write BHTTP response header");
            let _ = ohttp_writer.close().await;
            return;
        }

        let mut body_chunks = loopback_response.bytes_stream();
        while let Some(chunk_result) = body_chunks.next().await {
            match chunk_result {
                Ok(chunk) if chunk.is_empty() => continue,
                Ok(chunk) => {
                    if let Err(e) = bhttp_write_vec(&mut ohttp_writer, &chunk).await {
                        warn!(
                            error = %e,
                            "Failed to write BHTTP body chunk (client may have disconnected)"
                        );
                        let _ = ohttp_writer.close().await;
                        return;
                    }
                    if let Err(e) = ohttp_writer.flush().await {
                        warn!(error = %e, "Failed to flush OHTTP stream");
                        let _ = ohttp_writer.close().await;
                        return;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Error reading backend response stream");
                    break;
                }
            }
        }

        // Body terminator + empty trailer.
        if let Err(e) = bhttp_write_varint(&mut ohttp_writer, 0).await {
            warn!(error = %e, "Failed to write BHTTP body terminator");
            let _ = ohttp_writer.close().await;
            return;
        }
        if let Err(e) = bhttp_write_varint(&mut ohttp_writer, 0).await {
            warn!(error = %e, "Failed to write BHTTP trailer terminator");
            let _ = ohttp_writer.close().await;
            return;
        }
        if let Err(e) = ohttp_writer.close().await {
            warn!(error = %e, "Failed to close OHTTP stream");
        }
    });

    let body = Body::from_stream(tokio_util::io::ReaderStream::new(read_half));

    Ok((
        StatusCode::OK,
        [(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("message/ohttp-chunked-res"),
        )],
        body,
    )
        .into_response())
}

// ── BHTTP indeterminate-length framing helpers ───────────────────────────────

async fn bhttp_write_varint<W>(w: &mut W, v: u64) -> std::io::Result<()>
where
    W: futures_util::AsyncWrite + Unpin,
{
    if v < (1 << 6) {
        w.write_all(&[v as u8]).await
    } else if v < (1 << 14) {
        let bytes = ((v as u16) | (1 << 14)).to_be_bytes();
        w.write_all(&bytes).await
    } else if v < (1 << 30) {
        let bytes = ((v as u32) | (2 << 30)).to_be_bytes();
        w.write_all(&bytes).await
    } else if v < (1u64 << 62) {
        let bytes = (v | (3u64 << 62)).to_be_bytes();
        w.write_all(&bytes).await
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "BHTTP varint value too large",
        ))
    }
}

async fn bhttp_write_vec<W>(w: &mut W, bytes: &[u8]) -> std::io::Result<()>
where
    W: futures_util::AsyncWrite + Unpin,
{
    bhttp_write_varint(w, bytes.len() as u64).await?;
    if !bytes.is_empty() {
        w.write_all(bytes).await?;
    }
    Ok(())
}

async fn write_indeterminate_response_header<W>(
    w: &mut W,
    status: u16,
    headers: &[(Vec<u8>, Vec<u8>)],
) -> std::io::Result<()>
where
    W: futures_util::AsyncWrite + Unpin,
{
    bhttp_write_varint(w, 3).await?; // framing indicator: response, indeterminate-length
    bhttp_write_varint(w, u64::from(status)).await?;
    for (name, value) in headers {
        bhttp_write_vec(w, name).await?;
        bhttp_write_vec(w, value).await?;
    }
    bhttp_write_varint(w, 0).await?; // header section terminator
    Ok(())
}

fn collect_response_headers(response: &reqwest::Response) -> Vec<(Vec<u8>, Vec<u8>)> {
    response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            let n = name.as_str();
            if n.eq_ignore_ascii_case("transfer-encoding")
                || n.eq_ignore_ascii_case("connection")
                || n.eq_ignore_ascii_case("content-length")
            {
                None
            } else {
                Some((n.as_bytes().to_vec(), value.as_bytes().to_vec()))
            }
        })
        .collect()
}

// ── Shared helpers ───────────────────────────────────────────────────────────

/// Parse a Binary HTTP request and build a loopback reqwest request targeting
/// `http://127.0.0.1:{server_port}{inner_path}`.
///
/// If `outer_authorization` is a Bearer token it overrides any `Authorization`
/// inside the BHTTP payload and scrubs `X-Request-Hash` (trusted-gateway semantics).
fn parse_bhttp_and_build_loopback(
    state: &AppState,
    bhttp_request: &[u8],
    outer_authorization: Option<&HeaderValue>,
) -> Result<(reqwest::RequestBuilder, String), OhttpError> {
    let inner_msg = bhttp::Message::read_bhttp(&mut Cursor::new(bhttp_request)).map_err(|e| {
        warn!(error = %e, "Failed to parse Binary HTTP request");
        OhttpError::BadRequest(format!("Invalid Binary HTTP request: {e}"))
    })?;

    let control = inner_msg.control();
    let method_bytes = control.method().ok_or_else(|| {
        OhttpError::BadRequest("OHTTP inner message is not a request".to_string())
    })?;
    let path_bytes = control.path().unwrap_or(b"/");

    let method_str = std::str::from_utf8(method_bytes)
        .map_err(|_| OhttpError::BadRequest("Invalid method encoding".to_string()))?;
    let path_str = std::str::from_utf8(path_bytes)
        .map_err(|_| OhttpError::BadRequest("Invalid path encoding".to_string()))?;

    let method: Method = method_str
        .parse()
        .map_err(|_| OhttpError::BadRequest(format!("Unsupported HTTP method: {method_str}")))?;

    // Validate path before URL construction: must start with '/' (BHTTP origin-form,
    // RFC 9110 §7.1) and must not start with '//' (avoids protocol-relative parsing).
    // Concatenating an unvalidated path into format!("http://host{path}") allows
    // userinfo injection — e.g. path="@evil.com/x" → host=evil.com (SSRF).
    if !path_str.starts_with('/') || path_str.starts_with("//") {
        return Err(OhttpError::BadRequest(format!(
            "Invalid inner request path: must start with '/' and not '//' (got {path_str:?})"
        )));
    }

    let loopback_url = format!("http://127.0.0.1:{}{}", state.config.server.port, path_str);
    let mut request_builder = state.http_client.request(method, &loopback_url);

    // Treat outer `Authorization: Bearer …` as relay-injected — it overrides the
    // inner auth and scrubs trusted-only inner headers.
    let relay_outer_bearer =
        outer_authorization.filter(|v| v.to_str().is_ok_and(|h| h.starts_with("Bearer ")));

    for field in inner_msg.header().fields() {
        let name_bytes = field.name();
        let value_bytes = field.value();

        let skip = name_bytes.eq_ignore_ascii_case(b"host")
            || name_bytes.eq_ignore_ascii_case(b"transfer-encoding")
            || name_bytes.eq_ignore_ascii_case(b"connection")
            || (relay_outer_bearer.is_some() && name_bytes.eq_ignore_ascii_case(b"authorization"))
            || (relay_outer_bearer.is_some() && name_bytes.eq_ignore_ascii_case(b"x-request-hash"));

        if skip {
            continue;
        }

        match (
            HeaderName::from_bytes(name_bytes),
            HeaderValue::from_bytes(value_bytes),
        ) {
            (Ok(name), Ok(value)) => {
                request_builder = request_builder.header(name, value);
            }
            _ => {
                warn!(
                    name = %String::from_utf8_lossy(name_bytes),
                    "Skipping invalid inner OHTTP header"
                );
            }
        }
    }

    if let Some(auth) = relay_outer_bearer {
        request_builder = request_builder.header(header::AUTHORIZATION, auth.clone());
    }

    let inner_content = inner_msg.content().to_vec();
    if !inner_content.is_empty() {
        request_builder = request_builder.body(inner_content);
    }

    Ok((request_builder, path_str.to_string()))
}

async fn send_loopback(
    request_builder: reqwest::RequestBuilder,
) -> Result<reqwest::Response, OhttpError> {
    request_builder.send().await.map_err(|e| {
        warn!(error = %e, "OHTTP loopback request failed");
        OhttpError::Internal(format!("Loopback request failed: {e}"))
    })
}

fn copy_response_headers(response: &reqwest::Response, bhttp_msg: &mut bhttp::Message) {
    for (name, value) in response.headers() {
        let n = name.as_str();
        if !n.eq_ignore_ascii_case("transfer-encoding")
            && !n.eq_ignore_ascii_case("connection")
            && !n.eq_ignore_ascii_case("content-length")
        {
            bhttp_msg.put_header(n, value.as_bytes());
        }
    }
}
