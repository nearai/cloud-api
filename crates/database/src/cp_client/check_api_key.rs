//! `POST /v1/check_api_key` answered from
//! cloud-platform's verdict.
//!
//! A 200 (valid, with the credit gate passed) and a 402 (refused by the credit
//! gate) are cached for `CP_CHECK_API_KEY_TTL_SECS` with ±20% jitter, so
//! expiries do not synchronize. A 401 is not cached, so a newly created key is
//! accepted at once. Concurrent checks of one key make one call.
//!
//! When cloud-platform cannot answer, a cached verdict is served until it
//! expires; after that [`ApiKeyVerdicts::check`] returns an error, which the
//! route turns into a 503, never a 401, so an outage is not taken for a
//! revoked key.
//!
//! Cache keys are SHA-256 digests of the bearer token; the token itself is
//! never stored in the cache or logged.

use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use reqwest::{Method, StatusCode};
use sha2::{Digest, Sha256};

use super::cache::{KeyedCache, StaleUse};
use super::client::{CallType, CpClient, CpError, Credential};

const PATH: &[&str] = &["v1", "check_api_key"];

/// cloud-platform's answer for one key: its status and body, forwarded as is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub status: StatusCode,
    pub body: Bytes,
}

pub struct ApiKeyVerdicts {
    client: Arc<CpClient>,
    ttl: Duration,
    cache: KeyedCache<[u8; 32], Verdict>,
}

impl ApiKeyVerdicts {
    pub fn new(client: Arc<CpClient>, ttl: Duration) -> Self {
        Self {
            client,
            ttl,
            cache: KeyedCache::new(StaleUse::Never),
        }
    }

    /// The verdict for `bearer`. Fresh cached verdicts are returned without a
    /// call. Otherwise cloud-platform is asked, and an error means no verdict
    /// is available.
    pub async fn check(&self, bearer: &str) -> Result<Verdict, CpError> {
        let key: [u8; 32] = Sha256::digest(bearer.as_bytes()).into();
        let loaded = self
            .cache
            .get_or_load(key, || async {
                let response = self
                    .client
                    .send(
                        CallType::CheckApiKey,
                        Method::POST,
                        PATH,
                        &[],
                        Credential::Caller(bearer),
                        None,
                    )
                    .await?;
                match response.status {
                    StatusCode::OK | StatusCode::PAYMENT_REQUIRED => {
                        let verdict = Verdict {
                            status: response.status,
                            body: response.body,
                        };
                        Ok((verdict, jittered(self.ttl, unit_interval())))
                    }
                    // Not cached: a zero lifetime makes the next check reload.
                    StatusCode::UNAUTHORIZED => Ok((
                        Verdict {
                            status: response.status,
                            body: response.body,
                        },
                        Duration::ZERO,
                    )),
                    status => Err(CpError::Status(status)),
                }
            })
            .await?;
        Ok(loaded.value)
    }
}

/// `ttl` scaled by a factor in [0.8, 1.2): `unit` is in [0, 1).
fn jittered(ttl: Duration, unit: f64) -> Duration {
    ttl.mul_f64(0.8 + 0.4 * unit)
}

/// A random number in [0, 1). The source only has to spread expiries, so the
/// std hasher's per-instance random seed is enough.
fn unit_interval() -> f64 {
    let bits = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    (bits >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::super::client::tests::client;
    use super::*;
    use wiremock::matchers::{any, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn verdicts(server: &MockServer, ttl: Duration) -> Arc<ApiKeyVerdicts> {
        Arc::new(ApiKeyVerdicts::new(Arc::new(client(server)), ttl))
    }

    const VALID: &str =
        r#"{"valid":true,"organization_id":"o","workspace_id":"w","api_key_id":"k"}"#;

    #[test]
    fn jitter_stays_within_twenty_percent() {
        let ttl = Duration::from_secs(300);
        assert!(jittered(ttl, 0.0) >= Duration::from_millis(239_999));
        assert!(jittered(ttl, 0.999_999) < Duration::from_secs(360));
        assert!(jittered(ttl, 0.999_999) > Duration::from_secs(359));
        for _ in 0..10_000 {
            let got = jittered(ttl, unit_interval());
            assert!(got >= Duration::from_millis(239_999) && got < Duration::from_secs(360));
        }
    }

    #[tokio::test]
    async fn a_valid_verdict_is_cached_and_its_body_forwarded_byte_for_byte() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/check_api_key"))
            .and(header("authorization", "Bearer sk-caller"))
            .respond_with(ResponseTemplate::new(200).set_body_string(VALID))
            .expect(1)
            .mount(&server)
            .await;
        let checks = verdicts(&server, Duration::from_secs(60));
        for _ in 0..3 {
            let verdict = checks.check("sk-caller").await.unwrap();
            assert_eq!(verdict.status, StatusCode::OK);
            assert_eq!(verdict.body.as_ref(), VALID.as_bytes());
        }
    }

    #[tokio::test]
    async fn a_refusal_402_is_cached_with_its_body() {
        let server = MockServer::start().await;
        let body = r#"{"error":{"message":"no credits","type":"no_credits"}}"#;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(402).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;
        let checks = verdicts(&server, Duration::from_secs(60));
        let first = checks.check("sk-broke").await.unwrap();
        let second = checks.check("sk-broke").await.unwrap();
        assert_eq!(first.status, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(second, first);
    }

    #[tokio::test]
    async fn a_401_is_not_cached_so_a_new_key_is_accepted_at_once() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(401))
            .expect(2)
            .mount(&server)
            .await;
        let checks = verdicts(&server, Duration::from_secs(60));
        assert_eq!(
            checks.check("sk-new").await.unwrap().status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            checks.check("sk-new").await.unwrap().status,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn concurrent_checks_of_one_key_make_one_call() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(VALID)
                    .set_delay(Duration::from_millis(50)),
            )
            .expect(1)
            .mount(&server)
            .await;
        let checks = verdicts(&server, Duration::from_secs(60));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let checks = checks.clone();
            tasks.push(tokio::spawn(async move { checks.check("sk-hot").await }));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap().unwrap().status, StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn an_outage_serves_the_verdict_until_it_expires_then_refuses() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_string(VALID))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        // ttl 100 ms gives an expiry between 80 and 120 ms.
        let checks = verdicts(&server, Duration::from_millis(100));
        assert_eq!(
            checks.check("sk-outage").await.unwrap().status,
            StatusCode::OK
        );
        // Within the TTL the outage is invisible: the verdict is served.
        assert_eq!(
            checks.check("sk-outage").await.unwrap().status,
            StatusCode::OK
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Expired and unreachable: no verdict, so the route answers 503.
        assert!(checks.check("sk-outage").await.is_err());
    }

    #[tokio::test]
    async fn other_statuses_are_not_a_verdict() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let checks = verdicts(&server, Duration::from_secs(60));
        assert!(matches!(
            checks.check("sk-limited").await,
            Err(CpError::Status(StatusCode::TOO_MANY_REQUESTS))
        ));
    }
}
