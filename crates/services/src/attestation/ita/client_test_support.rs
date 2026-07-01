// allow: SIZE_OK - single fake ITA TCP server owns request capture and scripted transport failures.
use std::{
    collections::VecDeque,
    error::Error as StdError,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use config::{ItaAttestationConfig, ItaBaseUrl, ItaPolicyIds, ItaTokenSigningAlg};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

use super::ItaClient;
use crate::attestation::ita::{ItaAttestRequest, ItaVerifierNonce};

pub(crate) fn client_for(
    server: &FakeIta,
    max_retries: u32,
    timeout_ms: u64,
) -> Result<ItaClient, Box<dyn StdError>> {
    let config = config_for_api_base_url(&server.base_url(), max_retries)?;
    ItaClient::from_config_for_test(&config, Duration::from_millis(timeout_ms)).map_err(Into::into)
}

pub(crate) fn config_for_api_base_url(
    raw_api_base_url: &str,
    max_retries: u32,
) -> Result<ItaAttestationConfig, Box<dyn StdError>> {
    let api_base_url = ItaBaseUrl::parse(raw_api_base_url, "ITA_API_BASE_URL")?;
    let portal_base_url = ItaBaseUrl::parse("https://portal.example.test", "ITA_PORTAL_BASE_URL")?;
    Ok(ItaAttestationConfig {
        enabled: true,
        api_base_url,
        portal_base_url,
        api_key: Some("test-api-key".to_string()),
        timeout_seconds: 1,
        max_retries,
        retry_backoff_ms: 1,
        policy_ids: ItaPolicyIds::default(),
        policy_must_match: false,
        token_signing_alg: ItaTokenSigningAlg::Ps384,
    })
}

pub(crate) fn sample_attest_request() -> ItaAttestRequest {
    ItaAttestRequest {
        policy_ids: ItaPolicyIds::default(),
        token_signing_alg: ItaTokenSigningAlg::Ps384,
        policy_must_match: false,
        tdx: Some(crate::attestation::ita::ItaTdxEvidence {
            quote: "base64-quote".to_string(),
            runtime_data: "base64-runtime".to_string(),
            event_log: None,
            verifier_nonce: ItaVerifierNonce {
                val: "bm9uY2UtdmFs".to_string(),
                iat: "bm9uY2UtaWF0".to_string(),
                signature: "bm9uY2Utc2ln".to_string(),
            },
        }),
        nvgpu: None,
    }
}

#[derive(Clone)]
pub(crate) enum FakeStep {
    Response {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    },
    Hang,
    CloseBeforeResponse,
    ResetConnection,
}

impl FakeStep {
    pub(crate) fn json(status: u16, body: &str) -> Self {
        Self::Response {
            status,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: body.to_string(),
        }
    }

    pub(crate) fn body(status: u16, body: String) -> Self {
        Self::Response {
            status,
            headers: Vec::new(),
            body,
        }
    }

    pub(crate) fn with_header(self, name: &str, value: &str) -> Self {
        match self {
            Self::Response {
                status,
                mut headers,
                body,
            } => {
                headers.push((name.to_string(), value.to_string()));
                Self::Response {
                    status,
                    headers,
                    body,
                }
            }
            Self::Hang => Self::Hang,
            Self::CloseBeforeResponse => Self::CloseBeforeResponse,
            Self::ResetConnection => Self::ResetConnection,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RecordedRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) body: Vec<u8>,
    headers: Vec<(String, String)>,
}

impl RecordedRequest {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header_name, _)| header_name == name)
            .map(|(_, value)| value.as_str())
    }
}

pub(crate) struct FakeIta {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl FakeIta {
    pub(crate) async fn start(steps: Vec<FakeStep>) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shared_requests = Arc::clone(&requests);
        let shared_steps = Arc::new(Mutex::new(VecDeque::from(steps)));

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&shared_requests);
                let steps = Arc::clone(&shared_steps);
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut stream).await else {
                        return;
                    };
                    match requests.lock() {
                        Ok(mut requests) => requests.push(request),
                        Err(_) => return,
                    }
                    let step = match steps.lock() {
                        Ok(mut steps) => steps.pop_front().unwrap_or_else(|| {
                            FakeStep::json(500, r#"{"error":"unexpected request"}"#)
                        }),
                        Err(_) => return,
                    };
                    let _ = respond(&mut stream, step).await;
                });
            }
        });

        Ok(Self { addr, requests })
    }

    pub(crate) fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub(crate) fn requests(&self) -> Vec<RecordedRequest> {
        match self.requests.lock() {
            Ok(requests) => requests.clone(),
            Err(_) => Vec::new(),
        }
    }
}

async fn respond(stream: &mut tokio::net::TcpStream, step: FakeStep) -> io::Result<()> {
    match step {
        FakeStep::Response {
            status,
            headers,
            body,
        } => {
            let reason = if status == 200 { "OK" } else { "ERR" };
            let mut response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            );
            for (name, value) in headers {
                response.push_str(&format!("{name}: {value}\r\n"));
            }
            response.push_str("\r\n");
            response.push_str(&body);
            let _ = stream.write_all(response.as_bytes()).await;
            Ok(())
        }
        FakeStep::Hang => {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(())
        }
        FakeStep::CloseBeforeResponse => Ok(()),
        FakeStep::ResetConnection => stream.set_zero_linger(),
    }
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<RecordedRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = find_header_end(&buffer) {
            return parse_request(stream, buffer, header_end, &mut chunk).await;
        }
    }
}

async fn parse_request(
    stream: &mut tokio::net::TcpStream,
    mut buffer: Vec<u8>,
    header_end: usize,
    chunk: &mut [u8; 1024],
) -> Option<RecordedRequest> {
    let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let content_length = parse_content_length(&headers);
    let body_start = header_end + 4;
    while buffer.len() < body_start + content_length {
        let read = stream.read(chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let mut lines = headers.lines();
    let request_line = lines.next()?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next()?.to_string();
    let path = request_parts.next()?.to_string();
    let parsed_headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    Some(RecordedRequest {
        method,
        path,
        headers: parsed_headers,
        body: buffer[body_start..body_start + content_length].to_vec(),
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_content_length(headers: &str) -> usize {
    headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0)
}
