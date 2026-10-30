//! Catalog reads: the `ModelsRepository` reads served by cloud-platform's
//! internal catalog routes when the client is configured.
//!
//! - Model lists and names are cached for `CP_CATALOG_CACHE_TTL_SECS`. A failed
//!   reload serves the last good copy, without a time limit: these are listings
//!   and do not grant access.
//! - Model lookups that find a model are cached the same way (a miss is not
//!   cached, so a newly added model resolves at once), but an answer is used only while
//!   it is at most `CP_CATALOG_MAX_STALE_SECS - OUTER_RESOLVE_CACHE_TTL` old
//!   (measured from the fetch). The models service caches a resolved answer for
//!   a further `OUTER_RESOLVE_CACHE_TTL`, so this keeps the total within the
//!   bound. Past it the lookup fails. A lookup decides whether a model is
//!   listed, which gates the grant check, so a stale `is_listed` must not
//!   outlive that bound.
//!
//! End to end: a change to a model's listing or grants is honoured within at
//! most `CP_CATALOG_MAX_STALE_SECS` of the last successful fetch, including the
//! outer resolve cache. Startup refuses settings that leave no room for it.
//! - Model access is not cached here. The models service caches each decision
//!   for one TTL, which is the only bound on a grant. A failed read is an error,
//!   which fails the request closed and is not cached.
//!
//! Model access is answered only for the end caller's own API key, so it needs
//! the caller credential in scope ([`with_caller_credential`]). Without one the
//! read fails. `list_restricted_model_access` has no route and always reads the
//! local database.
//!
//! [`with_caller_credential`]: super::client::with_caller_credential

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use services::metrics::MetricsServiceTrait;
use services::models::{
    ModelWithPricing, ModelsRepository, RestrictedModelAccess, UpcomingModelPricing,
};
use services::usage::TextPricingProfile;
use uuid::Uuid;

use super::cache::{KeyedCache, Loaded, StaleUse};
use super::client::{
    current_caller_credential, CallType, CpClient, CpError, CpResponse, Credential, METRIC_CP_STALE,
};

const MODELS: &[&str] = &["v1", "internal", "catalog", "models"];
const LOOKUP: &[&str] = &["v1", "internal", "catalog", "model-lookup"];
const NAMES: &[&str] = &["v1", "internal", "catalog", "model-names"];
const ACCESS: &[&str] = &["v1", "internal", "catalog", "model-access"];

/// How long the models service's resolve cache keeps a resolved answer. It is
/// `MODEL_RESOLVE_CACHE_TTL_SECS` in `services/src/models/mod.rs`, a frozen
/// module this adapter cannot change. Keep the two equal: a lookup must not be
/// used past `CP_CATALOG_MAX_STALE_SECS` once that cache has held it.
pub const OUTER_RESOLVE_CACHE_TTL: Duration = Duration::from_secs(30);
const AREA_TAG: &str = "area:catalog";

/// `GET /v1/internal/catalog/models` item. Only the fields cloud-api reads;
/// unknown fields are ignored. `provider_config` arrives with `api_key`
/// redacted, which cloud-api's readers do not need (they read `backend`).
#[derive(Debug, Deserialize)]
struct CatalogModelDto {
    id: Uuid,
    model_name: String,
    model_display_name: String,
    model_description: String,
    #[serde(default)]
    model_icon: Option<String>,
    input_cost_per_token: i64,
    output_cost_per_token: i64,
    cost_per_image: i64,
    #[serde(default)]
    cache_read_cost_per_token: Option<i64>,
    #[serde(default)]
    text_pricing: Option<serde_json::Value>,
    #[serde(default)]
    upcoming_pricing: Option<UpcomingPricingDto>,
    context_length: i32,
    verifiable: bool,
    #[serde(default)]
    aliases: Vec<String>,
    owned_by: String,
    provider_type: String,
    #[serde(default)]
    provider_config: Option<serde_json::Value>,
    attestation_supported: bool,
    #[serde(default)]
    input_modalities: Option<Vec<String>>,
    #[serde(default)]
    output_modalities: Option<Vec<String>>,
    #[serde(default)]
    inference_url: Option<String>,
    #[serde(default)]
    hugging_face_id: Option<String>,
    #[serde(default)]
    quantization: Option<String>,
    #[serde(default)]
    max_output_length: Option<i32>,
    #[serde(default)]
    supported_sampling_parameters: Vec<String>,
    #[serde(default)]
    supported_features: Vec<String>,
    #[serde(default)]
    datacenters: Option<Vec<String>>,
    #[serde(default)]
    is_ready: Option<bool>,
    #[serde(default)]
    deprecation_date: Option<DateTime<Utc>>,
    #[serde(default)]
    successor_model_name: Option<String>,
    #[serde(default)]
    openrouter_slug: Option<String>,
    is_listed: bool,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct UpcomingPricingDto {
    effective_at: DateTime<Utc>,
    input_cost_per_token: i64,
    output_cost_per_token: i64,
    #[serde(default)]
    cache_read_cost_per_token: Option<i64>,
    cost_per_image: i64,
    #[serde(default)]
    text_pricing: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ModelAccessDto {
    has_access: bool,
}

fn parse_profile(value: Option<serde_json::Value>) -> Result<Option<TextPricingProfile>, CpError> {
    value
        .map(|value| serde_json::from_value(value).map_err(|_| CpError::Decode))
        .transpose()
}

impl CatalogModelDto {
    fn into_model(self) -> Result<ModelWithPricing, CpError> {
        let upcoming_pricing = match self.upcoming_pricing {
            None => None,
            Some(upcoming) => Some(UpcomingModelPricing {
                effective_at: upcoming.effective_at,
                input_cost_per_token: upcoming.input_cost_per_token,
                output_cost_per_token: upcoming.output_cost_per_token,
                cache_read_cost_per_token: upcoming.cache_read_cost_per_token,
                cost_per_image: upcoming.cost_per_image,
                text_pricing: parse_profile(upcoming.text_pricing)?,
            }),
        };
        Ok(ModelWithPricing {
            id: self.id,
            model_name: self.model_name,
            model_display_name: self.model_display_name,
            model_description: self.model_description,
            model_icon: self.model_icon,
            input_cost_per_token: self.input_cost_per_token,
            output_cost_per_token: self.output_cost_per_token,
            cost_per_image: self.cost_per_image,
            cache_read_cost_per_token: self.cache_read_cost_per_token,
            text_pricing: parse_profile(self.text_pricing)?,
            upcoming_pricing,
            context_length: self.context_length,
            verifiable: self.verifiable,
            aliases: self.aliases,
            owned_by: self.owned_by,
            provider_type: self.provider_type,
            provider_config: self.provider_config,
            attestation_supported: self.attestation_supported,
            input_modalities: self.input_modalities,
            output_modalities: self.output_modalities,
            inference_url: self.inference_url,
            hugging_face_id: self.hugging_face_id,
            quantization: self.quantization,
            max_output_length: self.max_output_length,
            supported_sampling_parameters: self.supported_sampling_parameters,
            supported_features: self.supported_features,
            datacenters: self.datacenters,
            is_ready: self.is_ready,
            deprecation_date: self.deprecation_date,
            successor_model_name: self.successor_model_name,
            openrouter_slug: self.openrouter_slug,
            is_listed: self.is_listed,
            created_at: self.created_at,
        })
    }
}

/// The `ModelsRepository` adapter for the catalog area.
pub struct CatalogModels {
    local: Arc<dyn ModelsRepository>,
    client: Arc<CpClient>,
    ttl: Duration,
    metrics: Option<Arc<dyn MetricsServiceTrait>>,
    models: KeyedCache<(), Vec<ModelWithPricing>>,
    lookups: KeyedCache<(String, bool), Option<ModelWithPricing>>,
    names: KeyedCache<(), Vec<String>>,
}

impl CatalogModels {
    pub fn new(
        local: Arc<dyn ModelsRepository>,
        client: Arc<CpClient>,
        ttl: Duration,
        max_stale: Duration,
        metrics: Option<Arc<dyn MetricsServiceTrait>>,
    ) -> Self {
        Self::with_outer_hold(
            local,
            client,
            ttl,
            max_stale,
            OUTER_RESOLVE_CACHE_TTL,
            metrics,
        )
    }

    /// `outer_hold` is how long the outer cache keeps a resolved answer; production
    /// passes [`OUTER_RESOLVE_CACHE_TTL`].
    fn with_outer_hold(
        local: Arc<dyn ModelsRepository>,
        client: Arc<CpClient>,
        ttl: Duration,
        max_stale: Duration,
        outer_hold: Duration,
        metrics: Option<Arc<dyn MetricsServiceTrait>>,
    ) -> Self {
        Self {
            local,
            client,
            ttl,
            metrics,
            models: KeyedCache::new(StaleUse::Forever),
            // Measured from the fetch, and reduced by the outer cache's hold.
            lookups: KeyedCache::new(StaleUse::MaxAge(max_stale.saturating_sub(outer_hold))),
            names: KeyedCache::new(StaleUse::Forever),
        }
    }

    fn record_stale(&self, loaded_stale: bool) {
        if loaded_stale {
            if let Some(metrics) = &self.metrics {
                metrics.record_count(METRIC_CP_STALE, 1, &[AREA_TAG]);
            }
        }
    }

    async fn remote_models(&self) -> Result<Vec<ModelWithPricing>, CpError> {
        let loaded = self
            .models
            .get_or_load((), || async {
                let response = self
                    .client
                    .send(
                        CallType::CatalogRead,
                        Method::GET,
                        MODELS,
                        &[],
                        Credential::Service,
                        None,
                    )
                    .await?;
                let response = expect_ok(response)?;
                let items: Vec<CatalogModelDto> = response.json()?;
                let models = items
                    .into_iter()
                    .map(CatalogModelDto::into_model)
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((models, self.ttl))
            })
            .await?;
        self.record_stale(loaded.stale);
        Ok(loaded.value)
    }

    async fn remote_lookup(
        &self,
        identifier: &str,
        resolve: bool,
    ) -> Result<Option<ModelWithPricing>, CpError> {
        let key = (identifier.to_string(), resolve);
        let loaded: Loaded<Option<ModelWithPricing>> = self
            .lookups
            .get_or_load(key, || async {
                let resolve = if resolve { "true" } else { "false" };
                let response = self
                    .client
                    .send(
                        CallType::CatalogRead,
                        Method::GET,
                        LOOKUP,
                        &[("identifier", identifier), ("resolve_alias", resolve)],
                        Credential::Service,
                        None,
                    )
                    .await?;
                match response.status {
                    // A miss is not cached (zero lifetime), so a model added
                    // after it resolves on the next lookup.
                    StatusCode::NOT_FOUND => Ok((None, Duration::ZERO)),
                    StatusCode::OK => {
                        let model = response.json::<CatalogModelDto>()?.into_model()?;
                        Ok((Some(model), self.ttl))
                    }
                    status => Err(CpError::Status(status)),
                }
            })
            .await?;
        self.record_stale(loaded.stale);
        Ok(loaded.value)
    }

    async fn remote_names(&self) -> Result<Vec<String>, CpError> {
        let loaded = self
            .names
            .get_or_load((), || async {
                let response = self
                    .client
                    .send(
                        CallType::CatalogRead,
                        Method::GET,
                        NAMES,
                        &[],
                        Credential::Service,
                        None,
                    )
                    .await?;
                let names: Vec<String> = expect_ok(response)?.json()?;
                Ok((names, self.ttl))
            })
            .await?;
        self.record_stale(loaded.stale);
        Ok(loaded.value)
    }

    /// Not cached here: the models service caches each decision for one TTL, the
    /// only bound on a grant. An expired or unanswered read is an error.
    async fn remote_access(&self, organization_id: Uuid, model_id: Uuid) -> Result<bool, CpError> {
        // The route answers only for the key's own organization, so the caller's
        // key is the only credential that can ask.
        let token = current_caller_credential().ok_or(CpError::NoCallerCredential)?;
        let organization = organization_id.to_string();
        let model = model_id.to_string();
        let response = self
            .client
            .send(
                CallType::ModelAccessRead,
                Method::GET,
                ACCESS,
                &[
                    ("organization_id", organization.as_str()),
                    ("model_id", model.as_str()),
                ],
                Credential::Caller(&token),
                None,
            )
            .await?;
        let answer: ModelAccessDto = expect_ok(response)?.json()?;
        Ok(answer.has_access)
    }
}

/// Any answer other than 200 is an error for a read.
fn expect_ok(response: CpResponse) -> Result<CpResponse, CpError> {
    match response.status {
        StatusCode::OK => Ok(response),
        status => Err(CpError::Status(status)),
    }
}

#[async_trait]
impl ModelsRepository for CatalogModels {
    async fn get_all_active_models(&self) -> anyhow::Result<Vec<ModelWithPricing>> {
        Ok(self.remote_models().await?)
    }

    async fn get_model_by_name(
        &self,
        model_name: &str,
    ) -> anyhow::Result<Option<ModelWithPricing>> {
        Ok(self.remote_lookup(model_name, false).await?)
    }

    async fn resolve_and_get_model(
        &self,
        identifier: &str,
    ) -> anyhow::Result<Option<ModelWithPricing>> {
        Ok(self.remote_lookup(identifier, true).await?)
    }

    async fn get_configured_model_names(&self) -> anyhow::Result<Vec<String>> {
        Ok(self.remote_names().await?)
    }

    async fn organization_has_model_access(
        &self,
        organization_id: Uuid,
        model_id: Uuid,
    ) -> anyhow::Result<bool> {
        // No answer is an error, not a denial: the request fails closed, and the
        // models service does not cache a failed lookup, so the next request
        // asks again instead of inheriting a transient "no access".
        match self.remote_access(organization_id, model_id).await {
            Ok(has_access) => Ok(has_access),
            Err(error) => {
                tracing::warn!(
                    error_kind = error.kind(),
                    "cloud-platform model access read failed"
                );
                Err(anyhow::Error::from(error))
            }
        }
    }

    async fn list_restricted_model_access(&self) -> anyhow::Result<Vec<RestrictedModelAccess>> {
        // No cloud-platform route lists grants; this read stays local.
        self.local.list_restricted_model_access().await
    }
}

#[cfg(test)]
mod tests {
    use super::super::client::tests::client;
    use super::super::client::with_caller_credential;
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use services::metrics::capturing::CapturingMetricsService;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A local repository with fixed answers.
    struct FakeLocal {
        models: Vec<ModelWithPricing>,
        access: bool,
    }

    #[async_trait]
    impl ModelsRepository for FakeLocal {
        async fn get_all_active_models(&self) -> anyhow::Result<Vec<ModelWithPricing>> {
            Ok(self.models.clone())
        }
        async fn get_model_by_name(&self, name: &str) -> anyhow::Result<Option<ModelWithPricing>> {
            Ok(self.models.iter().find(|m| m.model_name == name).cloned())
        }
        async fn resolve_and_get_model(
            &self,
            name: &str,
        ) -> anyhow::Result<Option<ModelWithPricing>> {
            self.get_model_by_name(name).await
        }
        async fn get_configured_model_names(&self) -> anyhow::Result<Vec<String>> {
            Ok(self.models.iter().map(|m| m.model_name.clone()).collect())
        }
        async fn organization_has_model_access(&self, _: Uuid, _: Uuid) -> anyhow::Result<bool> {
            Ok(self.access)
        }
    }

    fn model_id() -> Uuid {
        Uuid::from_u128(7)
    }

    fn remote_json(name: &str, context_length: i32) -> serde_json::Value {
        json!({
            "id": model_id(),
            "model_name": name,
            "model_display_name": name,
            "model_description": "d",
            "input_cost_per_token": 1,
            "output_cost_per_token": 2,
            "cost_per_image": 0,
            "context_length": context_length,
            "verifiable": true,
            "owned_by": "x",
            "provider_type": "vllm",
            "attestation_supported": true,
            "is_listed": true,
            "created_at": "2026-01-01T00:00:00Z",
            // Extra fields from a newer cloud-platform are ignored.
            "some_future_field": {"ignored": true},
        })
    }

    fn adapter(
        server: &MockServer,
        local: Vec<ModelWithPricing>,
        ttl: Duration,
        metrics: Option<Arc<CapturingMetricsService>>,
    ) -> CatalogModels {
        adapter_bounded(server, local, ttl, Duration::from_secs(300), metrics)
    }

    fn adapter_bounded(
        server: &MockServer,
        local: Vec<ModelWithPricing>,
        ttl: Duration,
        max_stale: Duration,
        metrics: Option<Arc<CapturingMetricsService>>,
    ) -> CatalogModels {
        adapter_with_outer_hold(server, local, ttl, max_stale, Duration::ZERO, metrics)
    }

    fn adapter_with_outer_hold(
        server: &MockServer,
        local: Vec<ModelWithPricing>,
        ttl: Duration,
        max_stale: Duration,
        outer_hold: Duration,
        metrics: Option<Arc<CapturingMetricsService>>,
    ) -> CatalogModels {
        let metrics: Option<Arc<dyn MetricsServiceTrait>> =
            metrics.map(|m| m as Arc<dyn MetricsServiceTrait>);
        CatalogModels::with_outer_hold(
            Arc::new(FakeLocal {
                models: local,
                access: true,
            }),
            Arc::new(client(server)),
            ttl,
            max_stale,
            outer_hold,
            metrics,
        )
    }

    #[tokio::test]
    async fn on_lookup_miss_is_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/catalog/model-lookup"))
            .and(query_param("identifier", "missing"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let catalog = adapter(&server, vec![], Duration::from_secs(30), None);
        assert!(catalog
            .get_model_by_name("missing")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn a_lookup_miss_is_not_cached_so_a_new_model_resolves_next_call() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/catalog/model-lookup"))
            .respond_with(ResponseTemplate::new(404))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/catalog/model-lookup"))
            .respond_with(ResponseTemplate::new(200).set_body_json(remote_json("new", 100)))
            .expect(1)
            .mount(&server)
            .await;
        let catalog = adapter(&server, vec![], Duration::from_secs(30), None);
        assert!(catalog
            .resolve_and_get_model("new")
            .await
            .unwrap()
            .is_none());
        let found = catalog.resolve_and_get_model("new").await.unwrap().unwrap();
        assert_eq!(found.model_name, "new");
        // The hit keeps its TTL: no further call.
        assert!(catalog
            .resolve_and_get_model("new")
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn on_list_is_cached_within_the_ttl() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/catalog/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([remote_json("m", 200)])))
            .expect(1)
            .mount(&server)
            .await;
        let catalog = adapter(&server, vec![], Duration::from_secs(30), None);
        for _ in 0..3 {
            let models = catalog.get_all_active_models().await.unwrap();
            assert_eq!(models[0].context_length, 200);
        }
    }

    #[tokio::test]
    async fn on_keeps_the_last_good_list_when_a_reload_fails() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/catalog/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([remote_json("m", 200)])))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/catalog/models"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let metrics = Arc::new(CapturingMetricsService::new());
        let catalog = adapter(
            &server,
            vec![],
            Duration::from_millis(20),
            Some(metrics.clone()),
        );
        assert_eq!(
            catalog.get_all_active_models().await.unwrap()[0].context_length,
            200
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
        let stale = catalog.get_all_active_models().await.unwrap();
        assert_eq!(stale[0].context_length, 200);
        assert!(metrics
            .get_metrics()
            .iter()
            .any(|metric| metric.name == METRIC_CP_STALE));
    }

    #[tokio::test]
    async fn on_without_a_copy_fails_and_the_breaker_opens() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/catalog/models"))
            .respond_with(ResponseTemplate::new(503))
            .expect(5)
            .mount(&server)
            .await;
        let catalog = adapter(&server, vec![], Duration::from_secs(30), None);
        for _ in 0..6 {
            assert!(catalog.get_all_active_models().await.is_err());
        }
        // The expect(5) above proves the sixth call never reached the server.
    }

    #[tokio::test]
    async fn model_access_needs_the_callers_key_and_is_not_cached_here() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/catalog/model-access"))
            .and(header("authorization", "Bearer sk-caller"))
            .and(query_param("model_id", model_id().to_string().as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"has_access": true})))
            .expect(2)
            .mount(&server)
            .await;
        let catalog = adapter(&server, vec![], Duration::from_secs(30), None);
        let org = Uuid::from_u128(9);

        // No caller key in scope: fail without calling cloud-platform.
        assert!(catalog
            .organization_has_model_access(org, model_id())
            .await
            .is_err());

        // Every decision is fetched: the models service owns the one-TTL cache.
        let granted = with_caller_credential("sk-caller".to_string(), async {
            (
                catalog
                    .organization_has_model_access(org, model_id())
                    .await
                    .unwrap(),
                catalog
                    .organization_has_model_access(org, model_id())
                    .await
                    .unwrap(),
            )
        })
        .await;
        assert_eq!(granted, (true, true));
    }

    #[tokio::test]
    async fn an_allow_is_not_served_stale_during_an_outage() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/catalog/model-access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"has_access": true})))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/catalog/model-access"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let catalog = adapter(&server, vec![], Duration::from_millis(50), None);
        let org = Uuid::from_u128(9);
        let check = || async {
            with_caller_credential("sk-caller".to_string(), async {
                catalog.organization_has_model_access(org, model_id()).await
            })
            .await
        };
        assert!(check().await.unwrap());
        // Cloud-platform is down: no earlier allow is reused, so the request fails closed.
        assert!(check().await.is_err());
    }

    #[tokio::test]
    async fn a_listed_model_past_the_stale_bound_fails_the_authorization_lookup() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/catalog/model-lookup"))
            .respond_with(ResponseTemplate::new(200).set_body_json(remote_json("m", 200)))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/catalog/model-lookup"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let catalog = adapter_bounded(
            &server,
            vec![],
            Duration::from_millis(50),
            Duration::from_millis(150),
            None,
        );
        assert!(catalog
            .get_model_by_name("m")
            .await
            .unwrap()
            .is_some_and(|model| model.is_listed));
        // Expired, within the bound: the last good lookup is served.
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(catalog.get_model_by_name("m").await.unwrap().is_some());
        // Past the bound since the fetch: the authorization path errors rather
        // than trusting a stale is_listed.
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(catalog.get_model_by_name("m").await.is_err());
    }

    #[tokio::test]
    async fn model_access_without_an_answer_is_an_error_not_a_cached_denial() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/catalog/model-access"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        let catalog = adapter(&server, vec![], Duration::from_secs(30), None);
        let answer = with_caller_credential("sk-caller".to_string(), async {
            catalog
                .organization_has_model_access(Uuid::from_u128(9), model_id())
                .await
        })
        .await;
        assert!(answer.is_err());
        // Not cached: once cloud-platform answers, the next read sees the grant.
        Mock::given(method("GET"))
            .and(path("/v1/internal/catalog/model-access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"has_access": true})))
            .mount(&server)
            .await;
        let granted = with_caller_credential("sk-caller".to_string(), async {
            catalog
                .organization_has_model_access(Uuid::from_u128(9), model_id())
                .await
                .unwrap()
        })
        .await;
        assert!(granted);
    }

    #[tokio::test]
    async fn a_stale_lookup_stops_before_the_outer_resolve_cache_could_extend_it() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/catalog/model-lookup"))
            .respond_with(ResponseTemplate::new(200).set_body_json(remote_json("m", 200)))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/catalog/model-lookup"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        // A 400 ms bound with a 50 ms outer hold leaves 350 ms of stale serving,
        // so the lookup stops before the bound itself.
        let catalog = adapter_with_outer_hold(
            &server,
            vec![],
            Duration::from_millis(10),
            Duration::from_millis(400),
            Duration::from_millis(50),
            None,
        );
        assert!(catalog.get_model_by_name("m").await.unwrap().is_some());
        // Stale and inside the usable window: served.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(catalog.get_model_by_name("m").await.unwrap().is_some());
        // Past the usable window: an error, so the outer cache never holds it.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(catalog.get_model_by_name("m").await.is_err());
    }
}
