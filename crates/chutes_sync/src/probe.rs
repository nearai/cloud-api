//! Network side of the sync: fetch the published feed, enumerate Chutes' TEE
//! chutes, fetch fresh evidence for each, and observe every instance.
//!
//! Errors are reported by HTTP status or category only, never with the
//! upstream body.

use std::future::Future;
use std::time::Duration;

use inference_providers::attested::chutes::client::{ChutesClient, ChutesClientError};
use inference_providers::attested::chutes::evidence::PublicInstanceEvidence;
use serde::Serialize;
use services::attestation::chutes::ChutesVerifyError;
use services::attestation::chutes_observer::ChutesObserver;
use services::attestation::chutes_pins::Registers;

use crate::classify::{parse_feed, FeedRow, Observation, ObservationOutcome, SkippedChute};

/// Verifies one instance's public evidence and returns its five registers.
/// The real implementation is [`ChutesObserver`] (DCAP quote, signed-body
/// freshness binding, NVIDIA NRAS); tests substitute a fake because those
/// checks need the network and a genuinely signed quote.
pub trait Observe {
    fn observe(
        &self,
        evidence: &PublicInstanceEvidence,
        nonce: &str,
    ) -> impl Future<Output = Result<Registers, ChutesVerifyError>> + Send;
}

impl Observe for ChutesObserver {
    fn observe(
        &self,
        evidence: &PublicInstanceEvidence,
        nonce: &str,
    ) -> impl Future<Output = Result<Registers, ChutesVerifyError>> + Send {
        self.observe_instance(evidence, nonce)
    }
}

pub const DEFAULT_FEED_URL: &str = "https://api.chutes.ai/servers/tee/measurements";

pub struct ProbeConfig {
    pub feed_url: String,
    /// Attempts per call (first try included) for transport errors, 429 and 5xx.
    pub attempts: u32,
    /// Backoff before attempt `n + 1` is `backoff * n`.
    pub backoff: Duration,
    /// Pause between chutes. Unauthenticated `/evidence` calls are
    /// rate-limited per client, so the probe spaces them out.
    pub pace: Duration,
    /// When set (`CHUTES_SYNC_MODELS`), probe only these model ids.
    pub only_models: Option<Vec<String>>,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            feed_url: DEFAULT_FEED_URL.to_string(),
            attempts: 4,
            backoff: Duration::from_secs(10),
            pace: Duration::from_secs(5),
            only_models: None,
        }
    }
}

/// Raw evidence kept for the audit artifact. Infrastructure data only.
#[derive(Debug, Clone, Serialize)]
pub struct QuoteRecord {
    pub model: String,
    pub chute_id: String,
    pub instance_id: String,
    pub nonce: String,
    pub quote_b64: String,
}

#[derive(Debug)]
pub struct ProbeOutput {
    pub feed: Vec<FeedRow>,
    pub observations: Vec<Observation>,
    pub skipped: Vec<SkippedChute>,
    pub quotes: Vec<QuoteRecord>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("measurement feed: {0}")]
    Feed(String),
    #[error("model list: {0}")]
    Models(String),
    #[error("every chute failed to return evidence")]
    AllChutesFailed,
    #[error("OS random number generator unavailable: {0}")]
    Rng(String),
}

/// Run `f` up to `attempts` times while `retryable` says the error is
/// transient, sleeping `backoff * n` before attempt `n + 1`.
async fn with_retry<T, E, F, Fut>(
    attempts: u32,
    backoff: Duration,
    retryable: impl Fn(&E) -> bool,
    mut f: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempt = 1;
    loop {
        match f().await {
            Err(e) if retryable(&e) && attempt < attempts => {
                tokio::time::sleep(backoff * attempt).await;
                attempt += 1;
            }
            other => return other,
        }
    }
}

fn client_retryable(e: &ChutesClientError) -> bool {
    match e {
        ChutesClientError::Http(_) => true,
        ChutesClientError::Status { status, .. } => *status == 429 || *status >= 500,
        ChutesClientError::BodyTooLarge { .. }
        | ChutesClientError::ModelNotFound(_)
        | ChutesClientError::MissingChuteId(_)
        | ChutesClientError::Decode { .. } => false,
    }
}

/// Status and category only: never the upstream body.
fn reason(e: &ChutesClientError) -> String {
    match e {
        ChutesClientError::Status { status, .. } => format!("HTTP {status}"),
        ChutesClientError::Http(_) => "transport error".to_string(),
        ChutesClientError::BodyTooLarge { .. } => "response too large".to_string(),
        ChutesClientError::Decode { what, .. } => format!("could not decode {what}"),
        ChutesClientError::ModelNotFound(_) | ChutesClientError::MissingChuteId(_) => {
            "model not listed".to_string()
        }
    }
}

/// A failed feed fetch: `status` is `None` for a transport or body error.
struct FeedFetchError {
    status: Option<u16>,
}

impl FeedFetchError {
    fn retryable(&self) -> bool {
        self.status.is_none_or(|s| s == 429 || s >= 500)
    }

    fn describe(&self) -> String {
        match self.status {
            Some(s) => format!("HTTP {s}"),
            None => "transport error".to_string(),
        }
    }
}

async fn fetch_feed(http: &reqwest::Client, url: &str) -> Result<String, FeedFetchError> {
    let transport = |_| FeedFetchError { status: None };
    let resp = http
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(transport)?;
    if !resp.status().is_success() {
        return Err(FeedFetchError {
            status: Some(resp.status().as_u16()),
        });
    }
    resp.text().await.map_err(transport)
}

fn random_nonce() -> Result<String, ProbeError> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| ProbeError::Rng(e.to_string()))?;
    Ok(hex::encode(b))
}

/// Probe every chute `/v1/models` lists (or only `cfg.only_models`) using
/// Chutes' public endpoints only (no API key). Fails
/// only if the feed or model list is unreachable, or every probed chute fails.
pub async fn run(
    client: &ChutesClient,
    http: &reqwest::Client,
    observer: &impl Observe,
    cfg: &ProbeConfig,
) -> Result<ProbeOutput, ProbeError> {
    let feed_body = with_retry(cfg.attempts, cfg.backoff, FeedFetchError::retryable, || {
        fetch_feed(http, &cfg.feed_url)
    })
    .await
    .map_err(|e| ProbeError::Feed(e.describe()))?;
    let feed = parse_feed(&feed_body).map_err(|e| ProbeError::Feed(e.to_string()))?;
    // An empty or wholly unparseable feed would make every carried row look
    // withdrawn and close the open bot PR; fail the run instead.
    if feed.is_empty() {
        return Err(ProbeError::Feed("no usable rows".to_string()));
    }

    let mut models = with_retry(cfg.attempts, cfg.backoff, client_retryable, || {
        client.list_models()
    })
    .await
    .map_err(|e| ProbeError::Models(reason(&e)))?;
    if let Some(only) = &cfg.only_models {
        models.retain(|(m, _)| only.contains(m));
        if models.is_empty() {
            return Err(ProbeError::Models(
                "none of the CHUTES_SYNC_MODELS ids are listed".to_string(),
            ));
        }
    }
    // An empty listing (schema change, permissions) must not pass as a run
    // that attested nothing.
    if models.is_empty() {
        return Err(ProbeError::Models("no chutes listed".to_string()));
    }

    let mut out = ProbeOutput {
        feed,
        observations: vec![],
        skipped: vec![],
        quotes: vec![],
    };
    let mut probed = 0usize;
    for (i, (model, chute_id)) in models.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(cfg.pace).await;
        }
        let nonce = random_nonce()?;
        let evidence = match with_retry(cfg.attempts, cfg.backoff, client_retryable, || {
            client.fetch_public_evidence(chute_id, &nonce)
        })
        .await
        {
            Ok(ev) => ev,
            Err(e) => {
                out.skipped.push(SkippedChute {
                    model: model.clone(),
                    chute_id: chute_id.clone(),
                    reason: format!("fetch evidence: {}", reason(&e)),
                });
                continue;
            }
        };
        probed += 1;
        let observation = |instance_id: &str, outcome| Observation {
            model: model.clone(),
            chute_id: chute_id.clone(),
            instance_id: instance_id.to_string(),
            outcome,
        };
        for failed in &evidence.failed_instance_ids {
            out.observations.push(observation(
                failed,
                ObservationOutcome::Failed {
                    stage: "evidence_unavailable".into(),
                },
            ));
        }
        for ev in &evidence.evidence {
            let instance_id = &ev.evidence.instance_id;
            out.quotes.push(QuoteRecord {
                model: model.clone(),
                chute_id: chute_id.clone(),
                instance_id: instance_id.clone(),
                nonce: nonce.clone(),
                quote_b64: ev.evidence.quote.clone(),
            });
            // One retry for the NVIDIA NRAS stage only (an external service).
            let result = with_retry(
                cfg.attempts.min(2),
                cfg.backoff,
                ChutesVerifyError::is_gpu_failure,
                || observer.observe(ev, &nonce),
            )
            .await;
            out.observations.push(observation(
                instance_id,
                match result {
                    Ok(registers) => ObservationOutcome::Verified(registers),
                    Err(e) => ObservationOutcome::Failed {
                        stage: e.stage().into(),
                    },
                },
            ));
        }
    }
    if probed == 0 {
        return Err(ProbeError::AllChutesFailed);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use inference_providers::attested::chutes::client::ChutesClient;
    use inference_providers::attested::chutes::evidence::PublicInstanceEvidence;
    use services::attestation::chutes_observer::ChutesObserver;
    use services::attestation::chutes_pins::{PinsFile, Registers};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::classify::ObservationOutcome;

    async fn get(server: &MockServer, p: &str, status: u16, body: &str) {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(status).set_body_string(body.to_string()))
            .mount(server)
            .await;
    }

    fn models(chutes: &[(&str, &str)]) -> String {
        let data: Vec<_> = chutes
            .iter()
            .map(|(m, c)| serde_json::json!({ "id": m, "chute_id": c }))
            .collect();
        serde_json::json!({ "data": data }).to_string()
    }

    const NULL_EVIDENCE: &str = r#"{"evidence": null}"#;
    const ONE_EVIDENCE: &str = r#"{"evidence": [{"quote": "q", "gpu_evidence": [], "instance_id": "i1", "certificate": "Y2VydA==", "signature": "c2ln", "attested_body": "Ym9keQ=="}]}"#;

    /// A feed with one well-formed row.
    fn feed() -> String {
        let r = "ab".repeat(48);
        serde_json::json!([{
            "version": "1.4.1",
            "name": "8xb300",
            "mrtd": r,
            "runtime_rtmrs": { "RTMR0": r, "RTMR1": r, "RTMR2": r, "RTMR3": r },
        }])
        .to_string()
    }

    fn cfg(server: &MockServer, only_models: Option<Vec<String>>) -> ProbeConfig {
        ProbeConfig {
            feed_url: format!("{}/servers/tee/measurements", server.uri()),
            attempts: 3,
            backoff: Duration::from_millis(1),
            pace: Duration::ZERO,
            only_models,
        }
    }

    fn client(server: &MockServer) -> ChutesClient {
        ChutesClient::public(5)
            .unwrap()
            .with_hosts(server.uri(), server.uri())
    }

    async fn probe_with(
        server: &MockServer,
        only_models: Option<Vec<String>>,
    ) -> Result<ProbeOutput, ProbeError> {
        run(
            &client(server),
            &reqwest::Client::new(),
            &ChutesObserver::new(None),
            &cfg(server, only_models),
        )
        .await
    }

    async fn probe(server: &MockServer) -> Result<ProbeOutput, ProbeError> {
        probe_with(server, None).await
    }

    /// Stands in for DCAP and NRAS, which need the network: returns fixed
    /// registers, optionally after one GPU-stage failure.
    struct FakeObserver {
        registers: Registers,
        fail_gpu_first: std::sync::atomic::AtomicBool,
    }

    impl Observe for FakeObserver {
        fn observe(
            &self,
            _evidence: &PublicInstanceEvidence,
            _nonce: &str,
        ) -> impl Future<Output = Result<Registers, ChutesVerifyError>> + Send {
            let fail = self
                .fail_gpu_first
                .swap(false, std::sync::atomic::Ordering::SeqCst);
            let registers = self.registers.clone();
            async move {
                if fail {
                    Err(ChutesVerifyError::MissingGpuVerdict)
                } else {
                    Ok(registers)
                }
            }
        }
    }

    #[tokio::test]
    async fn verified_instance_becomes_a_verified_observation() {
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, &feed()).await;
        get(&server, "/v1/models", 200, &models(&[("m1", "c1")])).await;
        get(&server, "/chutes/c1/evidence", 200, ONE_EVIDENCE).await;
        let r = "ab".repeat(48);
        let registers = Registers {
            mrtd: r.clone(),
            rtmr0: r.clone(),
            rtmr1: r.clone(),
            rtmr2: r.clone(),
            rtmr3: r,
        };
        let observer = FakeObserver {
            registers: registers.clone(),
            // The first attempt fails at the GPU stage; one retry follows.
            fail_gpu_first: true.into(),
        };
        let out = run(
            &client(&server),
            &reqwest::Client::new(),
            &observer,
            &cfg(&server, None),
        )
        .await
        .unwrap();
        assert_eq!(out.observations.len(), 1);
        assert_eq!(out.observations[0].instance_id, "i1");
        assert_eq!(
            out.observations[0].outcome,
            ObservationOutcome::Verified(registers.clone())
        );
        // And the classifier pins it: the feed fixture publishes these
        // registers.
        let (pins, _) = crate::classify::classify(
            &out.feed,
            &out.observations,
            &[],
            vec![],
            &PinsFile { families: vec![] },
        );
        assert!(pins.find(&registers).is_some());
    }

    #[tokio::test]
    async fn chute_with_no_instances_is_not_skipped() {
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, &feed()).await;
        get(&server, "/v1/models", 200, &models(&[("m1", "c1")])).await;
        get(&server, "/chutes/c1/evidence", 200, NULL_EVIDENCE).await;
        let out = probe(&server).await.unwrap();
        assert!(out.observations.is_empty());
        assert!(out.skipped.is_empty());
    }

    #[tokio::test]
    async fn instances_whose_evidence_failed_are_reported() {
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, &feed()).await;
        get(&server, "/v1/models", 200, &models(&[("m1", "c1")])).await;
        get(
            &server,
            "/chutes/c1/evidence",
            200,
            r#"{"evidence": [], "failed_instance_ids": ["i9"]}"#,
        )
        .await;
        let out = probe(&server).await.unwrap();
        assert_eq!(out.observations.len(), 1);
        assert_eq!(out.observations[0].instance_id, "i9");
        assert_eq!(
            out.observations[0].outcome,
            ObservationOutcome::Failed {
                stage: "evidence_unavailable".into()
            }
        );
    }

    #[tokio::test]
    async fn malformed_quote_is_unverified_at_transform() {
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, &feed()).await;
        get(&server, "/v1/models", 200, &models(&[("m1", "c1")])).await;
        get(
            &server,
            "/chutes/c1/evidence",
            200,
            &ONE_EVIDENCE.replace(r#""quote": "q""#, r#""quote": "!!!""#),
        )
        .await;
        let out = probe(&server).await.unwrap();
        assert_eq!(
            out.observations[0].outcome,
            ObservationOutcome::Failed {
                stage: "transform".into()
            }
        );
        assert_eq!(out.quotes.len(), 1);
        assert_eq!(out.quotes[0].instance_id, "i1");
    }

    #[tokio::test]
    async fn evidence_without_a_signed_body_is_unverified() {
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, &feed()).await;
        get(&server, "/v1/models", 200, &models(&[("m1", "c1")])).await;
        get(
            &server,
            "/chutes/c1/evidence",
            200,
            r#"{"evidence": [{"quote": "q", "gpu_evidence": [], "instance_id": "i1", "certificate": "Y2VydA=="}]}"#,
        )
        .await;
        let out = probe(&server).await.unwrap();
        assert_eq!(
            out.observations[0].outcome,
            ObservationOutcome::Failed {
                stage: "report_data".into()
            }
        );
    }

    #[tokio::test]
    async fn forbidden_chute_is_skipped_others_continue() {
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, &feed()).await;
        get(
            &server,
            "/v1/models",
            200,
            &models(&[("m1", "c1"), ("m2", "c2")]),
        )
        .await;
        get(&server, "/chutes/c1/evidence", 403, "nope").await;
        get(&server, "/chutes/c2/evidence", 200, NULL_EVIDENCE).await;
        let out = probe(&server).await.unwrap();
        assert_eq!(out.skipped.len(), 1);
        assert_eq!(out.skipped[0].model, "m1");
        assert!(out.skipped[0].reason.contains("403"));
        assert!(!out.skipped[0].reason.contains("nope"), "no upstream body");
    }

    #[tokio::test]
    async fn rate_limited_evidence_is_retried() {
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, &feed()).await;
        get(&server, "/v1/models", 200, &models(&[("m1", "c1")])).await;
        Mock::given(method("GET"))
            .and(path("/chutes/c1/evidence"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(2)
            .with_priority(1)
            .mount(&server)
            .await;
        get(&server, "/chutes/c1/evidence", 200, NULL_EVIDENCE).await;
        let out = probe(&server).await.unwrap();
        assert!(out.skipped.is_empty());
    }

    #[tokio::test]
    async fn every_chute_failing_is_an_error() {
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, &feed()).await;
        get(&server, "/v1/models", 200, &models(&[("m1", "c1")])).await;
        get(&server, "/chutes/c1/evidence", 500, "").await;
        assert!(matches!(
            probe(&server).await,
            Err(ProbeError::AllChutesFailed)
        ));
    }

    #[tokio::test]
    async fn unreachable_feed_is_an_error() {
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 503, "").await;
        assert!(matches!(probe(&server).await, Err(ProbeError::Feed(_))));
    }

    #[tokio::test]
    async fn feed_with_no_usable_rows_is_an_error() {
        // An empty or unparseable feed must fail the run rather than report
        // every published row as gone.
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, "[]").await;
        get(&server, "/v1/models", 200, &models(&[("m1", "c1")])).await;
        assert!(matches!(probe(&server).await, Err(ProbeError::Feed(_))));
    }

    #[tokio::test]
    async fn empty_model_list_is_an_error() {
        // A schema or permission change that empties /v1/models must not
        // produce a green run that attested nothing.
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, &feed()).await;
        get(&server, "/v1/models", 200, r#"{"data": []}"#).await;
        assert!(matches!(probe(&server).await, Err(ProbeError::Models(_))));
    }

    #[tokio::test]
    async fn transient_feed_5xx_is_retried_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/servers/tee/measurements"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .with_priority(1)
            .mount(&server)
            .await;
        get(&server, "/servers/tee/measurements", 200, &feed()).await;
        get(&server, "/v1/models", 200, &models(&[("m1", "c1")])).await;
        get(&server, "/chutes/c1/evidence", 200, NULL_EVIDENCE).await;
        assert!(probe(&server).await.is_ok());
    }

    #[tokio::test]
    async fn feed_retries_stop_after_the_configured_attempts() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/servers/tee/measurements"))
            .respond_with(ResponseTemplate::new(503))
            .expect(3)
            .mount(&server)
            .await;
        assert!(matches!(probe(&server).await, Err(ProbeError::Feed(_))));
    }

    #[tokio::test]
    async fn only_listed_models_are_probed() {
        // CHUTES_SYNC_MODELS limits the run to the listed models. c2 has no
        // mocks, so probing it would show up as a skipped chute.
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, &feed()).await;
        get(
            &server,
            "/v1/models",
            200,
            &models(&[("m1", "c1"), ("m2", "c2")]),
        )
        .await;
        get(&server, "/chutes/c1/evidence", 200, NULL_EVIDENCE).await;
        let out = probe_with(&server, Some(vec!["m1".into()])).await.unwrap();
        assert!(out.skipped.is_empty());
    }

    /// LIVE (ignored): run the keyless probe against real Chutes, DCAP
    /// collateral and NVIDIA NRAS for one model. Network only; no key.
    ///   cargo nextest run -p chutes_sync --run-ignored only -E 'test(live_)'
    /// Override the model with `CHUTES_PROBE_MODEL`.
    #[tokio::test]
    #[ignore]
    async fn live_keyless_probe_verifies_a_real_instance() {
        let model = std::env::var("CHUTES_PROBE_MODEL")
            .unwrap_or_else(|_| "moonshotai/Kimi-K3-TEE".to_string());
        let cfg = ProbeConfig {
            only_models: Some(vec![model]),
            ..ProbeConfig::default()
        };
        let out = run(
            &ChutesClient::public(60).unwrap(),
            &reqwest::Client::new(),
            &ChutesObserver::new(None),
            &cfg,
        )
        .await
        .expect("probe");
        eprintln!("{:#?}", out.observations);
        assert!(out
            .observations
            .iter()
            .any(|o| matches!(o.outcome, ObservationOutcome::Verified(_))));
    }
}
