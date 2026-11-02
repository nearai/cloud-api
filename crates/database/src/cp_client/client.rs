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
/// A read answered from the last good copy because cloud-platform failed.
pub const METRIC_CP_STALE: &str = "cloud_api.cp_client.stale_served";

tokio::task_local! {
    static REQUEST_ID: Uuid;
}

// The caller's key lives in `services` so request work spawned there (the
// Responses worker) can carry it into the spawned task.
pub use services::caller_credential::{current_caller_credential, with_caller_credential};

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

    /// The largest response body read for this call type. The catalog list is
    /// the one large answer; every other answer is a single small object.
    fn body_limit(self) -> usize {
        match self {
            Self::CatalogRead => 4 * 1024 * 1024,
            Self::ModelAccessRead | Self::SignatureRead | Self::CheckApiKey => 1024 * 1024,
        }
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

#[derive(Debug, Clone, thiserror::Error)]
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
    #[error("cloud-platform response body is too large")]
    TooLarge,
    #[error("no cloud-platform service credential configured")]
    NoCredential,
    #[error("no caller credential in scope for cloud-platform")]
    NoCallerCredential,
    #[error("invalid cloud-platform request")]
    InvalidRequest,
    #[error("too many cloud-platform calls in flight")]
    Overloaded,
}

impl CpError {
    /// A fixed label for logs and metrics. Never the error text, which can
    /// carry a URL.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::BreakerOpen => "breaker_open",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::Status(_) => "status",
            Self::Decode => "decode",
            Self::TooLarge => "too_large",
            Self::NoCredential => "no_credential",
            Self::NoCallerCredential => "no_caller_credential",
            Self::InvalidRequest => "invalid_request",
            Self::Overloaded => "overloaded",
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
    /// Advanced by every transition. A call records against the epoch it
    /// started in, so a completion from before the last transition is stale.
    epoch: u64,
}

/// What a call holds from `try_acquire` until it is recorded.
#[derive(Debug, Clone, Copy)]
struct Ticket {
    probe: bool,
    epoch: u64,
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
                epoch: 0,
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
    fn try_acquire(&self) -> Result<Ticket, ()> {
        let mut inner = self.inner.lock().unwrap();
        match Self::state_of(&inner, Instant::now()) {
            BreakerState::Closed => Ok(Ticket {
                probe: false,
                epoch: inner.epoch,
            }),
            BreakerState::Open => Err(()),
            BreakerState::HalfOpen if inner.probe_in_flight => Err(()),
            BreakerState::HalfOpen => {
                inner.probe_in_flight = true;
                Ok(Ticket {
                    probe: true,
                    epoch: inner.epoch,
                })
            }
        }
    }

    /// Records a finished call and returns the new state when it changed.
    ///
    /// Only the half-open probe closes an open breaker. A call from an older
    /// epoch (one that started before the breaker last changed state) is
    /// ignored, so a late success cannot close a breaker that opened after it
    /// started.
    fn record(&self, ticket: Ticket, success: bool) -> Option<BreakerState> {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        let before = Self::state_of(&inner, now);
        if ticket.probe {
            inner.probe_in_flight = false;
            if success {
                inner.consecutive_failures = 0;
                inner.open_until = None;
            } else {
                inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
                inner.open_until = Some(now + self.cooldown);
            }
            inner.epoch += 1;
        } else if ticket.epoch != inner.epoch {
            return None;
        } else if success {
            inner.consecutive_failures = 0;
        } else {
            inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
            if inner.consecutive_failures >= self.threshold {
                inner.open_until = Some(now + self.cooldown);
                inner.epoch += 1;
            }
        }
        let after = Self::state_of(&inner, now);
        (after != before || ticket.probe).then_some(after)
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
    /// Build the configured client, reading the service key file.
    pub fn from_config(
        config: &CpClientConfig,
        metrics: Option<Arc<dyn MetricsServiceTrait>>,
    ) -> anyhow::Result<Self> {
        let service_key = read_key_file(&config.api_key_file)?;
        Self::new(&config.base_url, Some(service_key), config, metrics)
    }

    pub fn new(
        base_url: &str,
        service_key: Option<String>,
        config: &CpClientConfig,
        metrics: Option<Arc<dyn MetricsServiceTrait>>,
    ) -> anyhow::Result<Self> {
        super::config::check_base_url(base_url).map_err(anyhow::Error::msg)?;
        let base_url = Url::parse(base_url)
            .map_err(|_| anyhow::anyhow!("CLOUD_PLATFORM_URL is not a valid URL"))?;
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
        let ticket = breaker.try_acquire().map_err(|()| CpError::BreakerOpen)?;
        if ticket.probe {
            self.breaker_changed(call, BreakerState::HalfOpen);
        }
        let mut probe_guard = ProbeGuard {
            breaker,
            ticket,
            armed: ticket.probe,
        };
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
            let body = read_body(response, call.body_limit()).await?;
            if status.is_server_error() {
                return Err(CpError::Status(status));
            }
            Ok(CpResponse { status, body })
        }
        .await;
        probe_guard.armed = false;
        if let Some(state) = breaker.record(ticket, outcome.is_ok()) {
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

/// Settles a half-open probe whose call is dropped before it completes (the
/// caller went away). The probe counts as a failure, so the breaker reopens
/// for a fresh cooldown instead of staying "probe in flight" forever.
struct ProbeGuard<'a> {
    breaker: &'a Breaker,
    ticket: Ticket,
    armed: bool,
}

impl Drop for ProbeGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.breaker.record(self.ticket, false);
        }
    }
}

/// Whether cloud-platform is configured (both connection settings present).
/// While false, request paths skip every cloud-platform scope and credential
/// copy. It only ever turns on: startup marks it when the client is built, and
/// nothing turns it off, so servers sharing a process (tests) cannot clear it
/// under each other. With the client unbuilt the scopes it enables are unread.
static CONFIGURED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn mark_configured() {
    CONFIGURED.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub fn configured() -> bool {
    CONFIGURED.load(std::sync::atomic::Ordering::Relaxed)
}

// The reqwest error is not logged or returned: its text carries the URL.
fn classify(error: reqwest::Error) -> CpError {
    if error.is_timeout() {
        CpError::Timeout
    } else {
        CpError::Transport
    }
}

/// Reads the body, refusing one larger than `limit` bytes whatever the
/// status. The declared length is checked first; the body is then read in
/// chunks and refused as soon as it passes the limit, so an oversized answer
/// is never held in full.
async fn read_body(mut response: reqwest::Response, limit: usize) -> Result<Bytes, CpError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(CpError::TooLarge);
    }
    let mut body = bytes::BytesMut::new();
    while let Some(chunk) = response.chunk().await.map_err(classify)? {
        if body.len() + chunk.len() > limit {
            return Err(CpError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
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
            anyhow::bail!("CLOUD_PLATFORM_API_KEY_FILE must be owner-only (0400 or 0600)");
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
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(3000)))
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
        assert!(started.elapsed() < Duration::from_millis(2000));
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

    #[tokio::test]
    async fn a_cancelled_probe_reopens_the_breaker_instead_of_wedging_it() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/fail"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(5)
            .mount(&server)
            .await;
        // The probe hangs past the caller's deadline and is dropped mid-flight.
        Mock::given(method("GET"))
            .and(path("/fail"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(800)))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/fail"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let client = client(&server);
        let call = || {
            client.send(
                CallType::CatalogRead,
                Method::GET,
                &["fail"],
                &[],
                Credential::Service,
                None,
            )
        };
        for _ in 0..5 {
            assert!(call().await.is_err());
        }
        assert_eq!(
            client.breaker_state(CallType::CatalogRead),
            BreakerState::Open
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            client.breaker_state(CallType::CatalogRead),
            BreakerState::HalfOpen
        );
        let dropped = tokio::time::timeout(Duration::from_millis(50), call()).await;
        assert!(dropped.is_err(), "the probe should still be in flight");
        // Not wedged: the dropped probe reopened the breaker for a fresh cooldown.
        assert_eq!(
            client.breaker_state(CallType::CatalogRead),
            BreakerState::Open
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            client.breaker_state(CallType::CatalogRead),
            BreakerState::HalfOpen
        );
        assert!(call().await.is_ok());
        assert_eq!(
            client.breaker_state(CallType::CatalogRead),
            BreakerState::Closed
        );
    }

    #[tokio::test]
    async fn an_oversized_body_is_refused_whatever_the_status_and_counts_as_a_failure() {
        let server = MockServer::start().await;
        let oversized = vec![b'x'; CallType::SignatureRead.body_limit() + 1];
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(ResponseTemplate::new(404).set_body_bytes(oversized))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/limit"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(vec![b'x'; CallType::SignatureRead.body_limit()]),
            )
            .mount(&server)
            .await;
        let client = client(&server);
        let call = |segments: &'static [&'static str]| {
            client.send(
                CallType::SignatureRead,
                Method::GET,
                segments,
                &[],
                Credential::Service,
                None,
            )
        };
        // Exactly at the limit is read.
        assert_eq!(
            call(&["limit"]).await.unwrap().body.len(),
            CallType::SignatureRead.body_limit()
        );
        for _ in 0..5 {
            assert!(matches!(call(&["big"]).await, Err(CpError::TooLarge)));
        }
        assert_eq!(
            client.breaker_state(CallType::SignatureRead),
            BreakerState::Open
        );
    }

    #[tokio::test]
    async fn read_body_reads_up_to_the_limit_and_refuses_past_it() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 64]))
            .mount(&server)
            .await;
        let response = reqwest::get(server.uri()).await.unwrap();
        assert!(matches!(
            read_body(response, 63).await,
            Err(CpError::TooLarge)
        ));
        let response = reqwest::get(server.uri()).await.unwrap();
        assert_eq!(read_body(response, 64).await.unwrap().len(), 64);
    }

    #[test]
    fn failed_probe_reopens() {
        let breaker = Breaker::new(1, Duration::from_millis(0));
        let first = breaker.try_acquire().unwrap();
        breaker.record(first, false);
        // Cooldown 0: immediately half-open, one probe only.
        let probe = breaker.try_acquire().unwrap();
        assert!(probe.probe);
        assert_eq!(breaker.try_acquire().map(|t| t.probe), Err(()));
        breaker.record(probe, false);
        assert!(breaker.inner.lock().unwrap().open_until.is_some());
    }

    #[test]
    fn a_late_success_does_not_close_an_opened_breaker() {
        let breaker = Breaker::new(1, Duration::from_millis(200));
        // Two calls start while the breaker is closed; one fails and opens it.
        let early = breaker.try_acquire().unwrap();
        let failing = breaker.try_acquire().unwrap();
        assert_eq!(breaker.record(failing, false), Some(BreakerState::Open));
        // The early call succeeds late: it must not close the breaker.
        assert_eq!(breaker.record(early, true), None);
        assert_eq!(breaker.state(), BreakerState::Open);
        std::thread::sleep(Duration::from_millis(250));
        // Only the half-open probe closes it.
        let probe = breaker.try_acquire().unwrap();
        assert!(probe.probe);
        assert_eq!(breaker.record(probe, true), Some(BreakerState::Closed));
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
