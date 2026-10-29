//! One shared HTTP client for cloud-platform calls: per-call-type deadlines
//! and circuit breakers, request-id propagation and metrics.
//!
//! Neither credentials nor bodies are ever logged or put in metric labels.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use reqwest::{Method, StatusCode, Url};
use services::metrics::MetricsServiceTrait;
use uuid::Uuid;

use super::config::CpClientConfig;

pub const METRIC_CP_CALLS: &str = "cloud_api.cp_client.calls";
pub const METRIC_CP_ERRORS: &str = "cloud_api.cp_client.errors";
pub const METRIC_CP_LATENCY: &str = "cloud_api.cp_client.latency";
/// 0 closed, 1 half-open, 2 open; recorded on every transition.
pub const METRIC_CP_BREAKER_STATE: &str = "cloud_api.cp_client.breaker_state";

tokio::task_local! {
    static REQUEST_ID: Uuid;
}

/// Run `fut` with `request_id` sent as `x-request-id` on every
/// cloud-platform call it makes. Calls outside a scope get a fresh id.
pub async fn with_request_id<F: std::future::Future>(request_id: Uuid, fut: F) -> F::Output {
    REQUEST_ID.scope(request_id, fut).await
}

fn current_request_id() -> Uuid {
    REQUEST_ID
        .try_with(|id| *id)
        .unwrap_or_else(|_| Uuid::new_v4())
}

/// Each call type has its own deadline class and breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallType {
    CatalogRead,
    ModelAccessRead,
    SignatureRead,
    CheckApiKey,
}

impl CallType {
    const ALL: [CallType; 4] = [
        CallType::CatalogRead,
        CallType::ModelAccessRead,
        CallType::SignatureRead,
        CallType::CheckApiKey,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::CatalogRead => "catalog_read",
            Self::ModelAccessRead => "model_access_read",
            Self::SignatureRead => "signature_read",
            Self::CheckApiKey => "check_api_key",
        }
    }

    fn index(self) -> usize {
        self as usize
    }

    fn is_write(self) -> bool {
        // Increment 1 has reads only; writes get `write_timeout` when added.
        false
    }
}

/// Whose credential a call carries as `Authorization: Bearer`.
pub enum Credential<'a> {
    /// The end caller's own credential, forwarded unchanged.
    Caller(&'a str),
    /// cloud-api's own API key, for calls with no end caller.
    Service,
}

#[derive(Debug, thiserror::Error)]
pub enum CpError {
    #[error("cloud-platform breaker open")]
    BreakerOpen,
    #[error("cloud-platform call timed out")]
    Timeout,
    #[error("cloud-platform unreachable")]
    Transport,
    #[error("cloud-platform returned {0}")]
    Status(StatusCode),
    #[error("cloud-platform response could not be decoded")]
    Decode,
    #[error("no cloud-platform service credential configured")]
    NoCredential,
    #[error("invalid cloud-platform request")]
    InvalidRequest,
}

impl CpError {
    fn kind(&self) -> &'static str {
        match self {
            Self::BreakerOpen => "breaker_open",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::Status(_) => "status",
            Self::Decode => "decode",
            Self::NoCredential => "no_credential",
            Self::InvalidRequest => "invalid_request",
        }
    }
}

/// A cloud-platform answer that reached us: any status below 500.
pub struct CpResponse {
    pub status: StatusCode,
    pub body: Bytes,
}

impl CpResponse {
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, CpError> {
        serde_json::from_slice(&self.body).map_err(|_| CpError::Decode)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    HalfOpen,
    Open,
}

impl BreakerState {
    fn gauge(self) -> f64 {
        match self {
            Self::Closed => 0.0,
            Self::HalfOpen => 1.0,
            Self::Open => 2.0,
        }
    }
}

#[derive(Debug)]
struct BreakerInner {
    consecutive_failures: u32,
    open_until: Option<Instant>,
    probe_in_flight: bool,
}

/// Opens after `threshold` consecutive failures; after `cooldown` lets one
/// probe through (half-open), which closes it on success or reopens it.
#[derive(Debug)]
pub struct Breaker {
    threshold: u32,
    cooldown: Duration,
    inner: Mutex<BreakerInner>,
}

impl Breaker {
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            threshold,
            cooldown,
            inner: Mutex::new(BreakerInner {
                consecutive_failures: 0,
                open_until: None,
                probe_in_flight: false,
            }),
        }
    }

    pub fn state(&self) -> BreakerState {
        let inner = self.inner.lock().unwrap();
        Self::state_of(&inner, Instant::now())
    }

    fn state_of(inner: &BreakerInner, now: Instant) -> BreakerState {
        match inner.open_until {
            None => BreakerState::Closed,
            Some(until) if now < until => BreakerState::Open,
            Some(_) => BreakerState::HalfOpen,
        }
    }

    /// Whether a call may go out now; a half-open breaker admits one probe.
    fn try_acquire(&self) -> Result<Option<BreakerState>, ()> {
        let mut inner = self.inner.lock().unwrap();
        match Self::state_of(&inner, Instant::now()) {
            BreakerState::Closed => Ok(None),
            BreakerState::Open => Err(()),
            BreakerState::HalfOpen if inner.probe_in_flight => Err(()),
            BreakerState::HalfOpen => {
                inner.probe_in_flight = true;
                Ok(Some(BreakerState::HalfOpen))
            }
        }
    }

    /// Returns the new state when it changed.
    fn record(&self, success: bool) -> Option<BreakerState> {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        let before = Self::state_of(&inner, now);
        let was_probe = std::mem::take(&mut inner.probe_in_flight);
        if success {
            inner.consecutive_failures = 0;
            inner.open_until = None;
        } else {
            inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
            if was_probe || inner.consecutive_failures >= self.threshold {
                inner.open_until = Some(now + self.cooldown);
            }
        }
        let after = Self::state_of(&inner, now);
        (after != before || was_probe).then_some(after)
    }
}

/// Shared cloud-platform client. `Debug` never prints the service key.
pub struct CpClient {
    http: reqwest::Client,
    base_url: Url,
    service_key: Option<String>,
    read_timeout: Duration,
    write_timeout: Duration,
    breakers: [Breaker; 4],
    metrics: Option<Arc<dyn MetricsServiceTrait>>,
}

impl std::fmt::Debug for CpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CpClient")
            .field("base_url", &self.base_url.as_str())
            .field("service_key", &self.service_key.as_ref().map(|_| "***"))
            .finish_non_exhaustive()
    }
}

impl CpClient {
    /// Build the client, reading the service key file when one is configured.
    pub fn from_config(
        config: &CpClientConfig,
        metrics: Option<Arc<dyn MetricsServiceTrait>>,
    ) -> anyhow::Result<Self> {
        let service_key = match &config.api_key_file {
            Some(path) => Some(read_key_file(path)?),
            None => None,
        };
        let base_url = config
            .base_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("CLOUD_PLATFORM_URL is not set"))?;
        Self::new(base_url, service_key, config, metrics)
    }

    pub fn new(
        base_url: &str,
        service_key: Option<String>,
        config: &CpClientConfig,
        metrics: Option<Arc<dyn MetricsServiceTrait>>,
    ) -> anyhow::Result<Self> {
        let base_url = Url::parse(base_url)
            .map_err(|_| anyhow::anyhow!("CLOUD_PLATFORM_URL is not a valid URL"))?;
        if base_url.cannot_be_a_base() || !matches!(base_url.scheme(), "http" | "https") {
            anyhow::bail!("CLOUD_PLATFORM_URL must be an http(s) base URL");
        }
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let breaker = || Breaker::new(config.breaker_failures, config.breaker_open);
        Ok(Self {
            http,
            base_url,
            service_key,
            read_timeout: config.read_timeout,
            write_timeout: config.write_timeout,
            breakers: [breaker(), breaker(), breaker(), breaker()],
            metrics,
        })
    }

    pub fn breaker_state(&self, call: CallType) -> BreakerState {
        self.breakers[call.index()].state()
    }

    fn url(&self, segments: &[&str], query: &[(&str, &str)]) -> Result<Url, CpError> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|_| CpError::InvalidRequest)?
            .pop_if_empty()
            .extend(segments);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    /// Send one call. Path segments and query values are percent-encoded.
    /// A 5xx, a timeout or a transport failure counts against the call
    /// type's breaker and is returned as an error; any other status is
    /// returned for the caller to interpret.
    pub async fn send(
        &self,
        call: CallType,
        method: Method,
        segments: &[&str],
        query: &[(&str, &str)],
        credential: Credential<'_>,
        body: Option<&serde_json::Value>,
    ) -> Result<CpResponse, CpError> {
        let started = Instant::now();
        let result = self
            .send_inner(call, method, segments, query, credential, body)
            .await;
        self.observe(call, started.elapsed(), &result);
        result
    }

    async fn send_inner(
        &self,
        call: CallType,
        method: Method,
        segments: &[&str],
        query: &[(&str, &str)],
        credential: Credential<'_>,
        body: Option<&serde_json::Value>,
    ) -> Result<CpResponse, CpError> {
        let bearer = match credential {
            Credential::Caller(token) => token,
            Credential::Service => self.service_key.as_deref().ok_or(CpError::NoCredential)?,
        };
        let url = self.url(segments, query)?;
        let breaker = &self.breakers[call.index()];
        match breaker.try_acquire() {
            Err(()) => return Err(CpError::BreakerOpen),
            Ok(Some(state)) => self.breaker_changed(call, state),
            Ok(None) => {}
        }
        let timeout = if call.is_write() {
            self.write_timeout
        } else {
            self.read_timeout
        };
        let mut request = self
            .http
            .request(method, url)
            .timeout(timeout)
            .bearer_auth(bearer)
            .header("x-request-id", current_request_id().to_string());
        if let Some(body) = body {
            request = request.json(body);
        }
        let outcome = async {
            let response = request.send().await.map_err(classify)?;
            let status = response.status();
            let body = response.bytes().await.map_err(classify)?;
            if status.is_server_error() {
                return Err(CpError::Status(status));
            }
            Ok(CpResponse { status, body })
        }
        .await;
        if let Some(state) = breaker.record(outcome.is_ok()) {
            self.breaker_changed(call, state);
        }
        outcome
    }

    fn breaker_changed(&self, call: CallType, state: BreakerState) {
        let call_tag = format!("call:{}", call.as_str());
        if state == BreakerState::Open {
            tracing::warn!(call = call.as_str(), "cloud-platform breaker opened");
        }
        if let Some(metrics) = &self.metrics {
            metrics.record_histogram(METRIC_CP_BREAKER_STATE, state.gauge(), &[&call_tag]);
        }
    }

    fn observe(&self, call: CallType, elapsed: Duration, result: &Result<CpResponse, CpError>) {
        let Some(metrics) = &self.metrics else {
            return;
        };
        let call_tag = format!("call:{}", call.as_str());
        let outcome = if result.is_ok() { "ok" } else { "error" };
        let outcome_tag = format!("outcome:{outcome}");
        metrics.record_count(METRIC_CP_CALLS, 1, &[&call_tag, &outcome_tag]);
        metrics.record_latency(METRIC_CP_LATENCY, elapsed, &[&call_tag]);
        if let Err(error) = result {
            let kind_tag = format!("kind:{}", error.kind());
            metrics.record_count(METRIC_CP_ERRORS, 1, &[&call_tag, &kind_tag]);
        }
    }

    /// Current state of every breaker, for health reporting.
    pub fn breaker_states(&self) -> Vec<(&'static str, BreakerState)> {
        CallType::ALL
            .iter()
            .map(|call| (call.as_str(), self.breaker_state(*call)))
            .collect()
    }
}

// The reqwest error is not logged or returned: its text carries the URL.
fn classify(error: reqwest::Error) -> CpError {
    if error.is_timeout() {
        CpError::Timeout
    } else {
        CpError::Transport
    }
}

/// Read the service API key, refusing a file that group or others can read.
fn read_key_file(path: &std::path::Path) -> anyhow::Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map_err(|_| anyhow::anyhow!("CLOUD_PLATFORM_API_KEY_FILE is not readable"))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            anyhow::bail!("CLOUD_PLATFORM_API_KEY_FILE must have mode 0600");
        }
    }
    let key = std::fs::read_to_string(path)
        .map_err(|_| anyhow::anyhow!("CLOUD_PLATFORM_API_KEY_FILE is not readable"))?;
    let key = key.trim().to_string();
    if key.is_empty() {
        anyhow::bail!("CLOUD_PLATFORM_API_KEY_FILE is empty");
    }
    Ok(key)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use services::metrics::capturing::CapturingMetricsService;
    use wiremock::matchers::{header, header_exists, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    pub(crate) fn test_config() -> CpClientConfig {
        CpClientConfig {
            breaker_open: Duration::from_millis(200),
            ..CpClientConfig::default()
        }
    }

    pub(crate) fn client(server: &MockServer) -> CpClient {
        CpClient::new(
            &server.uri(),
            Some("sk-service".to_string()),
            &test_config(),
            None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn sends_bearer_request_id_and_encoded_path() {
        let server = MockServer::start().await;
        let request_id = Uuid::new_v4();
        Mock::given(method("GET"))
            .and(path("/v1/internal/chat-signatures/a%2Fb"))
            .and(query_param("signing_algo", "ecdsa"))
            .and(header("authorization", "Bearer sk-caller"))
            .and(header("x-request-id", request_id.to_string().as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;
        let client = client(&server);
        let response = with_request_id(
            request_id,
            client.send(
                CallType::SignatureRead,
                Method::GET,
                &["v1", "internal", "chat-signatures", "a/b"],
                &[("signing_algo", "ecdsa")],
                Credential::Caller("sk-caller"),
                None,
            ),
        )
        .await
        .unwrap();
        assert_eq!(response.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn service_credential_and_fresh_request_id_outside_a_scope() {
        let server = MockServer::start().await;
        Mock::given(header("authorization", "Bearer sk-service"))
            .and(header_exists("x-request-id"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let response = client(&server)
            .send(
                CallType::CatalogRead,
                Method::GET,
                &["v1"],
                &[],
                Credential::Service,
                None,
            )
            .await
            .unwrap();
        assert_eq!(response.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn service_credential_is_required() {
        let server = MockServer::start().await;
        let client = CpClient::new(&server.uri(), None, &test_config(), None).unwrap();
        let result = client
            .send(
                CallType::CatalogRead,
                Method::GET,
                &["v1"],
                &[],
                Credential::Service,
                None,
            )
            .await;
        assert!(matches!(result, Err(CpError::NoCredential)));
    }

    #[tokio::test]
    async fn read_deadline_is_enforced() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(800)))
            .mount(&server)
            .await;
        let started = Instant::now();
        let result = client(&server)
            .send(
                CallType::CatalogRead,
                Method::GET,
                &["v1"],
                &[],
                Credential::Service,
                None,
            )
            .await;
        assert!(matches!(result, Err(CpError::Timeout)));
        assert!(started.elapsed() < Duration::from_millis(700));
    }

    #[tokio::test]
    async fn breaker_opens_after_five_failures_per_call_type_and_half_opens() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/fail"))
            .respond_with(ResponseTemplate::new(503))
            .expect(5)
            .mount(&server)
            .await;
        let metrics = Arc::new(CapturingMetricsService::new());
        let client = CpClient::new(
            &server.uri(),
            Some("sk-service".to_string()),
            &test_config(),
            Some(metrics.clone()),
        )
        .unwrap();
        let call = |segments: &'static [&'static str], call: CallType| {
            client.send(call, Method::GET, segments, &[], Credential::Service, None)
        };
        for _ in 0..5 {
            assert!(matches!(
                call(&["fail"], CallType::CatalogRead).await,
                Err(CpError::Status(_))
            ));
        }
        assert_eq!(
            client.breaker_state(CallType::CatalogRead),
            BreakerState::Open
        );
        // Open: no request reaches the server (the mock expects exactly 5).
        assert!(matches!(
            call(&["fail"], CallType::CatalogRead).await,
            Err(CpError::BreakerOpen)
        ));
        // Other call types keep their own breaker.
        assert_eq!(
            client.breaker_state(CallType::SignatureRead),
            BreakerState::Closed
        );

        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            client.breaker_state(CallType::CatalogRead),
            BreakerState::HalfOpen
        );
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        assert!(call(&["ok"], CallType::CatalogRead).await.is_ok());
        assert_eq!(
            client.breaker_state(CallType::CatalogRead),
            BreakerState::Closed
        );
        let names: Vec<String> = metrics
            .get_metrics()
            .into_iter()
            .map(|metric| metric.name)
            .collect();
        for expected in [
            METRIC_CP_CALLS,
            METRIC_CP_ERRORS,
            METRIC_CP_LATENCY,
            METRIC_CP_BREAKER_STATE,
        ] {
            assert!(names.iter().any(|name| name == expected), "{expected}");
        }
    }

    #[test]
    fn failed_probe_reopens() {
        let breaker = Breaker::new(1, Duration::from_millis(0));
        assert!(breaker.try_acquire().is_ok());
        breaker.record(false);
        // Cooldown 0: immediately half-open, one probe only.
        assert_eq!(breaker.try_acquire(), Ok(Some(BreakerState::HalfOpen)));
        assert_eq!(breaker.try_acquire(), Err(()));
        breaker.record(false);
        assert!(breaker.inner.lock().unwrap().open_until.is_some());
    }

    #[test]
    fn key_file_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("cp-key-{}", Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("key");
        std::fs::write(&file, "sk-service\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_key_file(&file).is_err());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_key_file(&file).unwrap(), "sk-service");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
