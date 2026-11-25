//! The organization policy read: one `GET /v1/internal/catalog/organization-policy`
//! serves the organization model filter and the long-context access inputs
//! when the client is configured.
//!
//! The route answers for the caller's own API key, so it needs the caller
//! credential in scope ([`with_caller_credential`]); without one the read
//! fails. Only a 200 and a 304 are successes. Every other status, a timeout,
//! a transport failure and an open breaker are errors.
//!
//! Callers keep their own caches (the models service's resolve cache, the
//! long-context access cache), so this source adds no lifetime of its own
//! beyond `ttl`, which wiring sets to zero: concurrent reads for one
//! organization share one call (single flight), and every later read
//! revalidates with `If-None-Match`. A 304 reuses the body held from the last
//! 200; the body is never served when the call fails, so an error is an error,
//! not an old answer.
//!
//! [`with_caller_credential`]: super::client::with_caller_credential

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Deserializer};
use services::long_context_access::{LongContextAccessRepository, LongContextDisabledPolicy};
use services::organization_model_filter::OrganizationModelFilterSource;
use uuid::Uuid;

use super::cache::{KeyedCache, StaleUse, MAX_CACHE_KEYS};
use super::client::{current_caller_credential, CallType, CpClient, CpError, Credential};

const POLICY: &[&str] = &["v1", "internal", "catalog", "organization-policy"];

/// Present in the body, `null` allowed: a missing `allowed_models` must not
/// read as "no filter".
fn required<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// The route's body. Only the fields cloud-api reads; unknown fields are
/// ignored. The fields it reads are required, so a changed contract fails
/// closed instead of reading as "no restriction".
#[derive(Debug, Deserialize)]
struct OrganizationPolicyDto {
    #[serde(deserialize_with = "required")]
    allowed_models: Option<Vec<String>>,
    long_context_disabled_models: Vec<String>,
    long_context_disabled_all: bool,
    blocked_model_names: Vec<String>,
}

#[derive(Debug)]
struct Policy {
    /// `None`: no filter.
    allowed_models: Option<Arc<HashSet<String>>>,
    long_context: LongContextDisabledPolicy,
    blocked_model_names: Vec<String>,
}

impl From<OrganizationPolicyDto> for Policy {
    fn from(dto: OrganizationPolicyDto) -> Self {
        let disabled_all = dto.long_context_disabled_all;
        Self {
            allowed_models: dto
                .allowed_models
                .map(|names| Arc::new(names.into_iter().collect())),
            long_context: LongContextDisabledPolicy {
                disabled_all,
                disabled_models: if disabled_all {
                    Vec::new()
                } else {
                    dto.long_context_disabled_models
                },
            },
            blocked_model_names: dto.blocked_model_names,
        }
    }
}

/// The last 200 for an organization, kept to revalidate with its ETag.
struct Held {
    etag: String,
    policy: Arc<Policy>,
}

pub struct OrganizationPolicySource {
    client: Arc<CpClient>,
    ttl: Duration,
    reads: KeyedCache<Uuid, Arc<Policy>, CpError>,
    held: Mutex<HashMap<Uuid, Held>>,
}

impl OrganizationPolicySource {
    pub fn new(client: Arc<CpClient>, ttl: Duration) -> Self {
        Self {
            client,
            ttl,
            // Never serve past expiry: an expired entry plus a failed call is
            // an error.
            reads: KeyedCache::new(StaleUse::Never),
            held: Mutex::new(HashMap::new()),
        }
    }

    async fn policy(&self, organization_id: Uuid) -> Result<Arc<Policy>, CpError> {
        // Taken before the cache so a read without a key fails at once, as
        // the model-access read does.
        let token = current_caller_credential().ok_or(CpError::NoCallerCredential)?;
        let loaded = self
            .reads
            .get_or_load(organization_id, || async {
                Ok((self.fetch(organization_id, &token).await?, self.ttl))
            })
            .await?;
        Ok(loaded.value)
    }

    async fn fetch(&self, organization_id: Uuid, token: &str) -> Result<Arc<Policy>, CpError> {
        let held_etag = self
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&organization_id)
            .map(|held| held.etag.clone());
        let headers: Vec<(&str, &str)> = held_etag
            .as_deref()
            .map(|etag| ("if-none-match", etag))
            .into_iter()
            .collect();
        let response = self
            .client
            .send_with_headers(
                CallType::OrganizationPolicyRead,
                Method::GET,
                POLICY,
                &[],
                &headers,
                Credential::Caller(token),
                None,
            )
            .await?;
        match response.status {
            StatusCode::OK => {
                let policy = Arc::new(Policy::from(response.json::<OrganizationPolicyDto>()?));
                let mut held = self
                    .held
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match response.etag {
                    Some(etag) => {
                        if held.len() >= MAX_CACHE_KEYS && !held.contains_key(&organization_id) {
                            held.clear();
                        }
                        held.insert(
                            organization_id,
                            Held {
                                etag,
                                policy: policy.clone(),
                            },
                        );
                    }
                    None => {
                        held.remove(&organization_id);
                    }
                }
                Ok(policy)
            }
            // A 304 answers only a request that carried the held ETag.
            StatusCode::NOT_MODIFIED => {
                let held = self
                    .held
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match (held.get(&organization_id), response.etag.as_deref()) {
                    (Some(held), etag)
                        if held_etag.is_some()
                            && etag.is_none_or(|etag| etag == held.etag.as_str()) =>
                    {
                        Ok(held.policy.clone())
                    }
                    _ => Err(CpError::Status(StatusCode::NOT_MODIFIED)),
                }
            }
            status => Err(CpError::Status(status)),
        }
    }
}

#[async_trait]
impl OrganizationModelFilterSource for OrganizationPolicySource {
    async fn allowed_models(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Option<Arc<HashSet<String>>>> {
        Ok(self.policy(organization_id).await?.allowed_models.clone())
    }
}

#[async_trait]
impl LongContextAccessRepository for OrganizationPolicySource {
    async fn blocked_model_names(&self, organization_id: Uuid) -> anyhow::Result<Vec<String>> {
        Ok(self
            .policy(organization_id)
            .await?
            .blocked_model_names
            .clone())
    }

    async fn long_context_disabled(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<LongContextDisabledPolicy> {
        Ok(self.policy(organization_id).await?.long_context.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cp_client::client::tests::{client, test_config};
    use crate::cp_client::client::with_caller_credential;
    use serde_json::json;
    use services::auth::ports::UserId;
    use services::organization::{Organization, OrganizationId};
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const PATH: &str = "/v1/internal/catalog/organization-policy";

    fn source(server: &MockServer) -> OrganizationPolicySource {
        OrganizationPolicySource::new(Arc::new(client(server)), Duration::ZERO)
    }

    fn body() -> serde_json::Value {
        json!({
            "allowed_models": ["a/b", "c/d"],
            "long_context_disabled_models": ["c/d"],
            "long_context_disabled_all": false,
            "blocked_model_names": ["e/f"],
            "restricted_model_access": [],
            "future_field": 1,
        })
    }

    async fn read(
        source: &OrganizationPolicySource,
        org: Uuid,
    ) -> anyhow::Result<Option<Arc<HashSet<String>>>> {
        with_caller_credential("sk-caller".to_string(), source.allowed_models(org)).await
    }

    #[tokio::test]
    async fn a_200_serves_both_consumers_and_a_304_reuses_the_held_body() {
        let server = MockServer::start().await;
        let org = Uuid::new_v4();
        Mock::given(method("GET"))
            .and(path(PATH))
            .and(header("authorization", "Bearer sk-caller"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"t1\"")
                    .set_body_json(body()),
            )
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(PATH))
            .and(header("if-none-match", "\"t1\""))
            .respond_with(ResponseTemplate::new(304).insert_header("etag", "\"t1\""))
            .expect(2)
            .mount(&server)
            .await;
        let source = source(&server);
        let allowed = read(&source, org).await.unwrap().unwrap();
        assert_eq!(allowed.len(), 2);
        // Expired at once, so this revalidates and the 304 keeps the body.
        let (blocked, disabled) = with_caller_credential("sk-caller".to_string(), async {
            (
                source.blocked_model_names(org).await.unwrap(),
                source.long_context_disabled(org).await.unwrap(),
            )
        })
        .await;
        assert_eq!(blocked, vec!["e/f".to_string()]);
        assert_eq!(
            disabled,
            LongContextDisabledPolicy {
                disabled_all: false,
                disabled_models: vec!["c/d".to_string()]
            }
        );
    }

    #[tokio::test]
    async fn a_new_200_replaces_the_held_body_and_etag() {
        let server = MockServer::start().await;
        let org = Uuid::new_v4();
        Mock::given(method("GET"))
            .and(path(PATH))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"t1\"")
                    .set_body_json(body()),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        let mut changed = body();
        changed["allowed_models"] = json!(null);
        Mock::given(method("GET"))
            .and(path(PATH))
            .and(header("if-none-match", "\"t1\""))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"t2\"")
                    .set_body_json(changed),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(PATH))
            .and(header("if-none-match", "\"t2\""))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;
        let source = source(&server);
        assert!(read(&source, org).await.unwrap().is_some());
        assert!(read(&source, org).await.unwrap().is_none());
        assert!(read(&source, org).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_error_after_a_good_answer_is_an_error_not_the_old_answer() {
        let server = MockServer::start().await;
        let org = Uuid::new_v4();
        Mock::given(method("GET"))
            .and(path(PATH))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"t1\"")
                    .set_body_json(body()),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(PATH))
            .and(header_exists("if-none-match"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let source = source(&server);
        assert!(read(&source, org).await.unwrap().is_some());
        assert!(read(&source, org).await.is_err());
    }

    #[tokio::test]
    async fn other_statuses_are_errors() {
        for status in [400, 401, 403, 404, 429, 500, 502] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
            assert!(
                read(&source(&server), Uuid::new_v4()).await.is_err(),
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn a_304_without_a_held_body_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(304).insert_header("etag", "\"t1\""))
            .mount(&server)
            .await;
        assert!(read(&source(&server), Uuid::new_v4()).await.is_err());
    }

    #[tokio::test]
    async fn a_timeout_and_a_missing_caller_credential_are_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(body())
                    .set_delay(Duration::from_millis(1500)),
            )
            .expect(1)
            .mount(&server)
            .await;
        let source = source(&server);
        assert!(read(&source, Uuid::new_v4()).await.is_err());
        // No key in scope: no call goes out (the mock expects exactly one).
        let error = source.allowed_models(Uuid::new_v4()).await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<CpError>(),
            Some(CpError::NoCallerCredential)
        ));
        let error = source
            .blocked_model_names(Uuid::new_v4())
            .await
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<CpError>(),
            Some(CpError::NoCallerCredential)
        ));
    }

    #[tokio::test]
    async fn an_open_breaker_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let source = source(&server);
        for _ in 0..test_config().breaker_failures {
            assert!(read(&source, Uuid::new_v4()).await.is_err());
        }
        let error = read(&source, Uuid::new_v4()).await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<CpError>(),
            Some(CpError::BreakerOpen)
        ));
    }

    #[tokio::test]
    async fn a_body_missing_a_field_cloud_api_reads_is_an_error() {
        for field in [
            "allowed_models",
            "long_context_disabled_models",
            "long_context_disabled_all",
            "blocked_model_names",
        ] {
            let server = MockServer::start().await;
            let mut partial = body();
            partial.as_object_mut().unwrap().remove(field);
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_json(partial))
                .mount(&server)
                .await;
            assert!(
                read(&source(&server), Uuid::new_v4()).await.is_err(),
                "{field}"
            );
        }
    }

    /// The body cloud-platform builds from stored settings (its documented
    /// semantics): `allowed_models` null/absent is no filter, a list of
    /// strings is the list, anything else is the empty list;
    /// `long_context_disabled_models` the same list rule (anything else
    /// empty) with `long_context_disabled_all` set when the stored value is
    /// malformed.
    fn cp_body_for(settings: &serde_json::Value) -> serde_json::Value {
        let names = |value: &serde_json::Value| -> Option<Vec<String>> {
            value.as_array().and_then(|entries| {
                entries
                    .iter()
                    .map(|entry| entry.as_str().map(str::to_string))
                    .collect()
            })
        };
        let allowed = match settings.get("allowed_models") {
            None | Some(serde_json::Value::Null) => json!(null),
            Some(value) => json!(names(value).unwrap_or_default()),
        };
        let (disabled, all) = match settings.get("long_context_disabled_models") {
            None | Some(serde_json::Value::Null) => (vec![], false),
            Some(value) => match names(value) {
                Some(list) => (list, false),
                None => (vec![], true),
            },
        };
        json!({
            "allowed_models": allowed,
            "long_context_disabled_models": disabled,
            "long_context_disabled_all": all,
            "blocked_model_names": [],
            "restricted_model_access": [],
        })
    }

    #[tokio::test]
    async fn cp_fields_give_the_same_decisions_as_the_stored_settings_parsers() {
        let probes = ["a/b", "c/d", "e/f"];
        let cases = [
            json!({}),
            json!({ "allowed_models": null, "long_context_disabled_models": null }),
            json!({ "allowed_models": ["a/b", "c/d"], "long_context_disabled_models": ["c/d"] }),
            json!({ "allowed_models": [], "long_context_disabled_models": [] }),
            json!({ "allowed_models": "a/b", "long_context_disabled_models": "c/d" }),
            json!({ "allowed_models": ["a/b", 7], "long_context_disabled_models": ["c/d", 7] }),
            json!({ "allowed_models": { "a/b": true }, "long_context_disabled_models": [null] }),
            json!({ "allowed_models": true, "long_context_disabled_models": true }),
        ];
        for settings in cases {
            let org_id = Uuid::new_v4();
            let organization = Organization {
                request_priority: 0,
                id: OrganizationId(org_id),
                name: "Example Org".to_string(),
                description: None,
                owner_id: UserId(Uuid::new_v4()),
                settings: settings.clone(),
                is_active: true,
                created_at: jiff::Timestamp::now(),
                updated_at: jiff::Timestamp::now(),
            };
            let local_allowed = organization.allowed_models();
            let local_disabled = organization.long_context_disabled_models();
            let local_policy = LongContextDisabledPolicy::from_settings(org_id, Some(&settings));

            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_json(cp_body_for(&settings)))
                .mount(&server)
                .await;
            let source = source(&server);
            let (cp_allowed, cp_policy) = with_caller_credential("sk-caller".to_string(), async {
                (
                    source.allowed_models(org_id).await.unwrap(),
                    source.long_context_disabled(org_id).await.unwrap(),
                )
            })
            .await;

            assert_eq!(
                cp_allowed.as_deref().cloned(),
                local_allowed,
                "allowed_models for {settings}"
            );
            assert_eq!(cp_policy, local_policy, "long context for {settings}");
            for model in probes {
                assert_eq!(
                    cp_policy.disabled_all
                        || cp_policy.disabled_models.iter().any(|name| name == model),
                    local_disabled.contains(model),
                    "{model} for {settings}"
                );
            }
        }
    }
}
