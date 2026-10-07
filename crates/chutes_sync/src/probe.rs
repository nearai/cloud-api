//! Network side of the sync: fetch the published feed, enumerate Chutes' TEE
//! chutes, fetch fresh evidence for each, and observe every instance.
//!
//! Errors are reported by HTTP status or category only, never with the
//! upstream body.

use std::future::Future;
use std::time::Duration;

use inference_providers::attested::chutes::client::{ChutesClient, ChutesClientError};
use serde::Serialize;
use services::attestation::chutes::{ChutesObserver, ChutesVerifyError};

use crate::classify::{parse_feed, FeedRow, Observation, ObservationOutcome, SkippedChute};

pub const DEFAULT_FEED_URL: &str = "https://api.chutes.ai/servers/tee/measurements";

pub struct ProbeConfig {
    pub feed_url: String,
    /// Attempts per call (first try included) for transport errors, 429 and 5xx.
    pub attempts: u32,
    /// Backoff before attempt `n + 1` is `backoff * n`.
    pub backoff: Duration,
    /// When set (`CHUTES_SYNC_MODELS`), probe only these model ids.
    pub only_models: Option<Vec<String>>,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            feed_url: DEFAULT_FEED_URL.to_string(),
            attempts: 3,
            backoff: Duration::from_secs(2),
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
    pub e2e_pubkey: String,
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
    #[error("every chute failed discovery or evidence")]
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

/// Probe every chute `/v1/models` lists (or only `cfg.only_models`). Fails
/// only if the feed or model list is unreachable, or every probed chute fails.
pub async fn run(
    client: &ChutesClient,
    http: &reqwest::Client,
    observer: &ChutesObserver,
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
    for (model, chute_id) in &models {
        let skip = |reason: String| SkippedChute {
            model: model.clone(),
            chute_id: chute_id.clone(),
            reason,
        };
        let instances = match with_retry(cfg.attempts, cfg.backoff, client_retryable, || {
            client.discover_instances(chute_id)
        })
        .await
        {
            Ok(i) => i.instances,
            Err(e) => {
                out.skipped
                    .push(skip(format!("discover instances: {}", reason(&e))));
                continue;
            }
        };
        let nonce = random_nonce()?;
        let evidence = match with_retry(cfg.attempts, cfg.backoff, client_retryable, || {
            client.fetch_evidence(chute_id, &nonce)
        })
        .await
        {
            Ok(ev) => ev,
            Err(e) => {
                out.skipped
                    .push(skip(format!("fetch evidence: {}", reason(&e))));
                continue;
            }
        };
        probed += 1;
        for inst in &instances {
            let pubkey = inst.e2e_pubkey.trim();
            let observation = |outcome| Observation {
                model: model.clone(),
                chute_id: chute_id.clone(),
                instance_id: inst.instance_id.clone(),
                outcome,
            };
            if pubkey.is_empty() {
                out.observations
                    .push(observation(ObservationOutcome::Failed {
                        stage: "no_e2e_pubkey".into(),
                    }));
                continue;
            }
            let Some(ev) = evidence.instance(&inst.instance_id) else {
                out.observations
                    .push(observation(ObservationOutcome::Failed {
                        stage: "missing_evidence".into(),
                    }));
                continue;
            };
            out.quotes.push(QuoteRecord {
                model: model.clone(),
                chute_id: chute_id.clone(),
                instance_id: inst.instance_id.clone(),
                nonce: nonce.clone(),
                e2e_pubkey: pubkey.to_string(),
                quote_b64: ev.quote.clone(),
            });
            // One retry for the NVIDIA NRAS stage only (an external service).
            let result = with_retry(
                cfg.attempts.min(2),
                cfg.backoff,
                ChutesVerifyError::is_gpu_failure,
                || observer.observe_instance(ev, &nonce, pubkey),
            )
            .await;
            out.observations.push(observation(match result {
                Ok(registers) => ObservationOutcome::Verified(registers),
                Err(e) => ObservationOutcome::Failed {
                    stage: e.stage().into(),
                },
            }));
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
    use services::attestation::chutes::ChutesObserver;
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

    const NO_INSTANCES: &str = r#"{"instances": []}"#;
    const ONE_INSTANCE: &str =
        r#"{"instances": [{"instance_id": "i1", "e2e_pubkey": "cGs=", "nonces": []}]}"#;
    const NULL_EVIDENCE: &str = r#"{"evidence": null}"#;

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

    async fn probe_with(
        server: &MockServer,
        only_models: Option<Vec<String>>,
    ) -> Result<ProbeOutput, ProbeError> {
        let client = ChutesClient::new("k".into(), 5)
            .unwrap()
            .with_hosts(server.uri(), server.uri());
        let cfg = ProbeConfig {
            feed_url: format!("{}/servers/tee/measurements", server.uri()),
            attempts: 3,
            backoff: Duration::from_millis(1),
            only_models,
        };
        run(
            &client,
            &reqwest::Client::new(),
            &ChutesObserver::new(None),
            &cfg,
        )
        .await
    }

    async fn probe(server: &MockServer) -> Result<ProbeOutput, ProbeError> {
        probe_with(server, None).await
    }

    #[tokio::test]
    async fn chute_with_no_instances_is_not_skipped() {
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, &feed()).await;
        get(&server, "/v1/models", 200, &models(&[("m1", "c1")])).await;
        get(&server, "/e2e/instances/c1", 200, NO_INSTANCES).await;
        get(&server, "/chutes/c1/evidence", 200, NULL_EVIDENCE).await;
        let out = probe(&server).await.unwrap();
        assert!(out.observations.is_empty());
        assert!(out.skipped.is_empty());
    }

    #[tokio::test]
    async fn instance_missing_from_evidence_is_unverified() {
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, &feed()).await;
        get(&server, "/v1/models", 200, &models(&[("m1", "c1")])).await;
        get(&server, "/e2e/instances/c1", 200, ONE_INSTANCE).await;
        get(&server, "/chutes/c1/evidence", 200, r#"{"evidence": []}"#).await;
        let out = probe(&server).await.unwrap();
        assert_eq!(out.observations.len(), 1);
        assert_eq!(out.observations[0].instance_id, "i1");
        assert_eq!(
            out.observations[0].outcome,
            ObservationOutcome::Failed {
                stage: "missing_evidence".into()
            }
        );
    }

    #[tokio::test]
    async fn malformed_quote_is_unverified_at_transform() {
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, &feed()).await;
        get(&server, "/v1/models", 200, &models(&[("m1", "c1")])).await;
        get(&server, "/e2e/instances/c1", 200, ONE_INSTANCE).await;
        get(
            &server,
            "/chutes/c1/evidence",
            200,
            r#"{"evidence": [{"quote": "!!!", "gpu_evidence": [], "instance_id": "i1", "certificate": "Y2VydA=="}]}"#,
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
        get(&server, "/e2e/instances/c1", 403, "nope").await;
        get(&server, "/e2e/instances/c2", 200, NO_INSTANCES).await;
        get(&server, "/chutes/c2/evidence", 200, NULL_EVIDENCE).await;
        let out = probe(&server).await.unwrap();
        assert_eq!(out.skipped.len(), 1);
        assert_eq!(out.skipped[0].model, "m1");
        assert!(out.skipped[0].reason.contains("403"));
        assert!(!out.skipped[0].reason.contains("nope"), "no upstream body");
    }

    #[tokio::test]
    async fn rate_limited_discovery_is_retried() {
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, &feed()).await;
        get(&server, "/v1/models", 200, &models(&[("m1", "c1")])).await;
        Mock::given(method("GET"))
            .and(path("/e2e/instances/c1"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(2)
            .with_priority(1)
            .mount(&server)
            .await;
        get(&server, "/e2e/instances/c1", 200, NO_INSTANCES).await;
        get(&server, "/chutes/c1/evidence", 200, NULL_EVIDENCE).await;
        let out = probe(&server).await.unwrap();
        assert!(out.skipped.is_empty());
    }

    #[tokio::test]
    async fn every_chute_failing_is_an_error() {
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, &feed()).await;
        get(&server, "/v1/models", 200, &models(&[("m1", "c1")])).await;
        get(&server, "/e2e/instances/c1", 500, "").await;
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
        // An empty or unparseable feed must fail the run: otherwise every
        // carried row looks withdrawn and the open bot PR gets closed.
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, "[]").await;
        get(&server, "/v1/models", 200, &models(&[("m1", "c1")])).await;
        assert!(matches!(probe(&server).await, Err(ProbeError::Feed(_))));
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
        get(&server, "/e2e/instances/c1", 200, NO_INSTANCES).await;
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
    async fn empty_model_list_is_an_error() {
        // A schema or permission change that empties /v1/models must not
        // produce a green run that attested nothing.
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, &feed()).await;
        get(&server, "/v1/models", 200, r#"{"data": []}"#).await;
        assert!(matches!(probe(&server).await, Err(ProbeError::Models(_))));
    }

    #[tokio::test]
    async fn instance_without_e2e_pubkey_is_reported() {
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, &feed()).await;
        get(&server, "/v1/models", 200, &models(&[("m1", "c1")])).await;
        get(
            &server,
            "/e2e/instances/c1",
            200,
            r#"{"instances": [{"instance_id": "i1", "e2e_pubkey": " ", "nonces": []}]}"#,
        )
        .await;
        get(&server, "/chutes/c1/evidence", 200, NULL_EVIDENCE).await;
        let out = probe(&server).await.unwrap();
        assert_eq!(
            out.observations[0].outcome,
            ObservationOutcome::Failed {
                stage: "no_e2e_pubkey".into()
            }
        );
    }

    #[tokio::test]
    async fn only_listed_models_are_probed() {
        // CHUTES_SYNC_MODELS fallback: if the key cannot read other chutes,
        // the probe is limited to the listed models. c2 has no mocks, so
        // probing it would show up as a skipped chute.
        let server = MockServer::start().await;
        get(&server, "/servers/tee/measurements", 200, &feed()).await;
        get(
            &server,
            "/v1/models",
            200,
            &models(&[("m1", "c1"), ("m2", "c2")]),
        )
        .await;
        get(&server, "/e2e/instances/c1", 200, NO_INSTANCES).await;
        get(&server, "/chutes/c1/evidence", 200, NULL_EVIDENCE).await;
        let out = probe_with(&server, Some(vec!["m1".into()])).await.unwrap();
        assert!(out.skipped.is_empty());
    }
}
