//! Catalog area (`CP_CATALOG`): the `ModelsRepository` reads, served by
//! cloud-platform's internal catalog routes.
//!
//! - `off`: the local repository answers, and cloud-platform is never called.
//! - `shadow`: the local answer is served and the cloud-platform answer is
//!   compared with it; each difference is counted by field.
//! - `on`: cloud-platform answers. Model lists, lookups and names are cached
//!   for `CP_CATALOG_CACHE_TTL_SECS`, and a failed reload serves the last good
//!   copy. Model access is cached the same way; a failed read is an error,
//!   which fails the request closed and is not cached.
//!
//! Model access is answered only for the end caller's own API key, so it
//! needs the caller credential in scope ([`with_caller_credential`]). Without
//! one it denies. `list_restricted_model_access` has no route and always
//! reads the local database.
//!
//! [`with_caller_credential`]: super::client::with_caller_credential

use std::collections::BTreeMap;
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
    current_caller_credential, CallType, CpClient, CpError, CpResponse, Credential,
    METRIC_CP_SHADOW_MISMATCH, METRIC_CP_STALE,
};
use super::config::AreaMode;

const MODELS: &[&str] = &["v1", "internal", "catalog", "models"];
const LOOKUP: &[&str] = &["v1", "internal", "catalog", "model-lookup"];
const NAMES: &[&str] = &["v1", "internal", "catalog", "model-names"];
const ACCESS: &[&str] = &["v1", "internal", "catalog", "model-access"];
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
    mode: AreaMode,
    local: Arc<dyn ModelsRepository>,
    client: Arc<CpClient>,
    ttl: Duration,
    metrics: Option<Arc<dyn MetricsServiceTrait>>,
    models: KeyedCache<(), Vec<ModelWithPricing>>,
    lookups: KeyedCache<(String, bool), Option<ModelWithPricing>>,
    names: KeyedCache<(), Vec<String>>,
    access: KeyedCache<(Uuid, Uuid), bool>,
}

impl CatalogModels {
    pub fn new(
        mode: AreaMode,
        local: Arc<dyn ModelsRepository>,
        client: Arc<CpClient>,
        ttl: Duration,
        metrics: Option<Arc<dyn MetricsServiceTrait>>,
    ) -> Self {
        Self {
            mode,
            local,
            client,
            ttl,
            metrics,
            models: KeyedCache::new(StaleUse::Forever),
            lookups: KeyedCache::new(StaleUse::Forever),
            names: KeyedCache::new(StaleUse::Forever),
            // Authorization: a last-good answer is served for at most one more TTL.
            access: KeyedCache::new(StaleUse::Bounded(ttl)),
        }
    }

    fn record_stale(&self, loaded_stale: bool) {
        if loaded_stale {
            if let Some(metrics) = &self.metrics {
                metrics.record_count(METRIC_CP_STALE, 1, &[AREA_TAG]);
            }
        }
    }

    fn record_mismatches(&self, fields: &[&'static str]) {
        let Some(metrics) = &self.metrics else {
            return;
        };
        for field in fields {
            let field_tag = format!("field:{field}");
            metrics.record_count(METRIC_CP_SHADOW_MISMATCH, 1, &[AREA_TAG, &field_tag]);
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
                    StatusCode::NOT_FOUND => Ok((None, self.ttl)),
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

    async fn remote_access(&self, organization_id: Uuid, model_id: Uuid) -> Result<bool, CpError> {
        let loaded = self
            .access
            .get_or_load((organization_id, model_id), || async {
                // The route answers only for the key's own organization, so the
                // caller's key is the only credential that can ask.
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
                Ok((answer.has_access, self.ttl))
            })
            .await?;
        self.record_stale(loaded.stale);
        Ok(loaded.value)
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
        match self.mode {
            AreaMode::Off => self.local.get_all_active_models().await,
            AreaMode::Shadow => {
                let local = self.local.get_all_active_models().await?;
                if let Ok(remote) = self.remote_models().await {
                    self.record_mismatches(&diff_lists(&local, &remote));
                }
                Ok(local)
            }
            AreaMode::On => Ok(self.remote_models().await?),
        }
    }

    async fn get_model_by_name(
        &self,
        model_name: &str,
    ) -> anyhow::Result<Option<ModelWithPricing>> {
        self.lookup(model_name, false).await
    }

    async fn resolve_and_get_model(
        &self,
        identifier: &str,
    ) -> anyhow::Result<Option<ModelWithPricing>> {
        self.lookup(identifier, true).await
    }

    async fn get_configured_model_names(&self) -> anyhow::Result<Vec<String>> {
        match self.mode {
            AreaMode::Off => self.local.get_configured_model_names().await,
            AreaMode::Shadow => {
                let local = self.local.get_configured_model_names().await?;
                if let Ok(remote) = self.remote_names().await {
                    if sorted(local.clone()) != sorted(remote) {
                        self.record_mismatches(&["model_names"]);
                    }
                }
                Ok(local)
            }
            AreaMode::On => Ok(self.remote_names().await?),
        }
    }

    async fn organization_has_model_access(
        &self,
        organization_id: Uuid,
        model_id: Uuid,
    ) -> anyhow::Result<bool> {
        match self.mode {
            AreaMode::Off => {
                self.local
                    .organization_has_model_access(organization_id, model_id)
                    .await
            }
            AreaMode::Shadow => {
                let local = self
                    .local
                    .organization_has_model_access(organization_id, model_id)
                    .await?;
                if let Ok(remote) = self.remote_access(organization_id, model_id).await {
                    if remote != local {
                        self.record_mismatches(&["has_access"]);
                    }
                }
                Ok(local)
            }
            // No answer is an error, not a denial: the request fails closed, and the
            // models service does not cache a failed lookup, so the next request
            // asks again instead of inheriting a transient "no access".
            AreaMode::On => match self.remote_access(organization_id, model_id).await {
                Ok(has_access) => Ok(has_access),
                Err(error) => {
                    tracing::warn!(
                        error_kind = error.kind(),
                        "cloud-platform model access read failed"
                    );
                    Err(anyhow::Error::from(error))
                }
            },
        }
    }

    async fn list_restricted_model_access(&self) -> anyhow::Result<Vec<RestrictedModelAccess>> {
        // No cloud-platform route lists grants; this read stays local.
        self.local.list_restricted_model_access().await
    }
}

impl CatalogModels {
    async fn lookup(
        &self,
        identifier: &str,
        resolve: bool,
    ) -> anyhow::Result<Option<ModelWithPricing>> {
        let local = || async {
            if resolve {
                self.local.resolve_and_get_model(identifier).await
            } else {
                self.local.get_model_by_name(identifier).await
            }
        };
        match self.mode {
            AreaMode::Off => local().await,
            AreaMode::Shadow => {
                let served = local().await?;
                if let Ok(remote) = self.remote_lookup(identifier, resolve).await {
                    self.record_mismatches(&diff_option(served.as_ref(), remote.as_ref()));
                }
                Ok(served)
            }
            AreaMode::On => Ok(self.remote_lookup(identifier, resolve).await?),
        }
    }
}

fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names
}

/// Field names that differ between two models. Remote `provider_config` is
/// redacted, so `api_key` is left out of the comparison on both sides.
fn diff_model(local: &ModelWithPricing, remote: &ModelWithPricing) -> Vec<&'static str> {
    let mut fields = Vec::new();
    macro_rules! compare {
        ($($field:ident),+ $(,)?) => {
            $(
                if local.$field != remote.$field {
                    fields.push(stringify!($field));
                }
            )+
        };
    }
    compare!(
        id,
        model_name,
        model_display_name,
        model_description,
        model_icon,
        input_cost_per_token,
        output_cost_per_token,
        cost_per_image,
        cache_read_cost_per_token,
        text_pricing,
        context_length,
        verifiable,
        aliases,
        owned_by,
        provider_type,
        attestation_supported,
        input_modalities,
        output_modalities,
        inference_url,
        hugging_face_id,
        quantization,
        max_output_length,
        supported_sampling_parameters,
        supported_features,
        datacenters,
        is_ready,
        deprecation_date,
        successor_model_name,
        openrouter_slug,
        is_listed,
        created_at,
    );
    if without_api_key(&local.provider_config) != without_api_key(&remote.provider_config) {
        fields.push("provider_config");
    }
    if upcoming_differs(&local.upcoming_pricing, &remote.upcoming_pricing) {
        fields.push("upcoming_pricing");
    }
    fields
}

fn without_api_key(config: &Option<serde_json::Value>) -> Option<serde_json::Value> {
    config.clone().map(|mut value| {
        if let Some(object) = value.as_object_mut() {
            object.remove("api_key");
        }
        value
    })
}

fn upcoming_differs(
    local: &Option<UpcomingModelPricing>,
    remote: &Option<UpcomingModelPricing>,
) -> bool {
    match (local, remote) {
        (None, None) => false,
        (Some(a), Some(b)) => {
            a.effective_at != b.effective_at
                || a.input_cost_per_token != b.input_cost_per_token
                || a.output_cost_per_token != b.output_cost_per_token
                || a.cache_read_cost_per_token != b.cache_read_cost_per_token
                || a.cost_per_image != b.cost_per_image
                || a.text_pricing != b.text_pricing
        }
        _ => true,
    }
}

fn diff_option(
    local: Option<&ModelWithPricing>,
    remote: Option<&ModelWithPricing>,
) -> Vec<&'static str> {
    match (local, remote) {
        (None, None) => Vec::new(),
        (Some(local), Some(remote)) => diff_model(local, remote),
        _ => vec!["presence"],
    }
}

/// Compares two model lists by name. A model present on one side only is
/// `model_set`; a model on both sides contributes its differing fields.
fn diff_lists(local: &[ModelWithPricing], remote: &[ModelWithPricing]) -> Vec<&'static str> {
    let local: BTreeMap<&str, &ModelWithPricing> = local
        .iter()
        .map(|model| (model.model_name.as_str(), model))
        .collect();
    let remote: BTreeMap<&str, &ModelWithPricing> = remote
        .iter()
        .map(|model| (model.model_name.as_str(), model))
        .collect();
    let mut fields = Vec::new();
    if local.keys().ne(remote.keys()) {
        fields.push("model_set");
    }
    for (name, local_model) in &local {
        if let Some(remote_model) = remote.get(name) {
            fields.extend(diff_model(local_model, remote_model));
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::super::client::tests::client;
    use super::super::client::with_caller_credential;
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use services::metrics::capturing::CapturingMetricsService;
    use wiremock::matchers::{any, header, method, path, query_param};
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

    fn created_at() -> DateTime<Utc> {
        "2026-01-01T00:00:00Z".parse().unwrap()
    }

    fn local_model(name: &str, context_length: i32) -> ModelWithPricing {
        ModelWithPricing {
            id: model_id(),
            model_name: name.to_string(),
            model_display_name: name.to_string(),
            model_description: "d".to_string(),
            model_icon: None,
            input_cost_per_token: 1,
            output_cost_per_token: 2,
            cost_per_image: 0,
            cache_read_cost_per_token: None,
            text_pricing: None,
            upcoming_pricing: None,
            context_length,
            verifiable: true,
            aliases: vec![],
            owned_by: "x".to_string(),
            provider_type: "vllm".to_string(),
            provider_config: None,
            attestation_supported: true,
            input_modalities: None,
            output_modalities: None,
            inference_url: None,
            hugging_face_id: None,
            quantization: None,
            max_output_length: None,
            supported_sampling_parameters: vec![],
            supported_features: vec![],
            datacenters: None,
            is_ready: None,
            deprecation_date: None,
            successor_model_name: None,
            openrouter_slug: None,
            is_listed: true,
            created_at: created_at(),
        }
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
        mode: AreaMode,
        local: Vec<ModelWithPricing>,
        ttl: Duration,
        metrics: Option<Arc<CapturingMetricsService>>,
    ) -> CatalogModels {
        let metrics: Option<Arc<dyn MetricsServiceTrait>> =
            metrics.map(|m| m as Arc<dyn MetricsServiceTrait>);
        CatalogModels::new(
            mode,
            Arc::new(FakeLocal {
                models: local,
                access: true,
            }),
            Arc::new(client(server)),
            ttl,
            metrics,
        )
    }

    #[tokio::test]
    async fn off_never_calls_cloud_platform() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let catalog = adapter(
            &server,
            AreaMode::Off,
            vec![local_model("m", 100)],
            Duration::from_secs(30),
            None,
        );
        let models = catalog.get_all_active_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert!(catalog.get_model_by_name("m").await.unwrap().is_some());
        assert!(catalog.resolve_and_get_model("m").await.unwrap().is_some());
        assert_eq!(catalog.get_configured_model_names().await.unwrap(), ["m"]);
        let _ = with_caller_credential(
            "sk-caller".to_string(),
            catalog.organization_has_model_access(Uuid::nil(), model_id()),
        )
        .await
        .unwrap();
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
        let catalog = adapter(&server, AreaMode::On, vec![], Duration::from_secs(30), None);
        assert!(catalog
            .get_model_by_name("missing")
            .await
            .unwrap()
            .is_none());
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
        let catalog = adapter(&server, AreaMode::On, vec![], Duration::from_secs(30), None);
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
            AreaMode::On,
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
        let catalog = adapter(&server, AreaMode::On, vec![], Duration::from_secs(30), None);
        for _ in 0..6 {
            assert!(catalog.get_all_active_models().await.is_err());
        }
        // The expect(5) above proves the sixth call never reached the server.
    }

    #[tokio::test]
    async fn shadow_serves_the_local_answer_and_counts_mismatches_by_field() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/catalog/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([remote_json("m", 200)])))
            .mount(&server)
            .await;
        let metrics = Arc::new(CapturingMetricsService::new());
        let catalog = adapter(
            &server,
            AreaMode::Shadow,
            vec![local_model("m", 100)],
            Duration::from_secs(30),
            Some(metrics.clone()),
        );
        let served = catalog.get_all_active_models().await.unwrap();
        assert_eq!(served[0].context_length, 100);
        let mismatches: Vec<String> = metrics
            .get_metrics()
            .into_iter()
            .filter(|metric| metric.name == METRIC_CP_SHADOW_MISMATCH)
            .flat_map(|metric| metric.tags)
            .filter(|tag| tag.starts_with("field:"))
            .collect();
        assert_eq!(mismatches, ["field:context_length"]);
    }

    #[tokio::test]
    async fn model_access_needs_the_callers_key_and_is_cached() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/catalog/model-access"))
            .and(header("authorization", "Bearer sk-caller"))
            .and(query_param("model_id", model_id().to_string().as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"has_access": true})))
            .expect(1)
            .mount(&server)
            .await;
        let catalog = adapter(&server, AreaMode::On, vec![], Duration::from_secs(30), None);
        let org = Uuid::from_u128(9);

        // No caller key in scope: fail without calling cloud-platform.
        assert!(catalog
            .organization_has_model_access(org, model_id())
            .await
            .is_err());

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
    async fn an_expired_allow_is_denied_once_the_stale_bound_passes() {
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
        let ttl = Duration::from_millis(50);
        let catalog = adapter(&server, AreaMode::On, vec![], ttl, None);
        let org = Uuid::from_u128(9);
        let check = || async {
            with_caller_credential("sk-caller".to_string(), async {
                catalog.organization_has_model_access(org, model_id()).await
            })
            .await
        };
        assert!(check().await.unwrap());
        // Expired, cloud-platform down, still inside one TTL of grace: the old allow holds.
        tokio::time::sleep(ttl + Duration::from_millis(10)).await;
        assert!(check().await.unwrap());
        // Past the grace bound: no answer, so the request fails closed.
        tokio::time::sleep(ttl + Duration::from_millis(40)).await;
        assert!(check().await.is_err());
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
        let catalog = adapter(&server, AreaMode::On, vec![], Duration::from_secs(30), None);
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
    async fn diff_lists_reports_membership_and_fields() {
        let local = vec![local_model("a", 1), local_model("b", 2)];
        let remote = vec![local_model("a", 9)];
        assert_eq!(diff_lists(&local, &remote), ["model_set", "context_length"]);
    }
}
