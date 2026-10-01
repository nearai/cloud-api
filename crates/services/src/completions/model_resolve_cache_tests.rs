use super::provider_attribution_tests::{test_model, StaticOrganizationLimitRepository};
use super::*;
use crate::inference_provider_pool::InferenceProviderPool;
use crate::metrics::capturing::CapturingMetricsService;
use crate::models::{
    model_resolve_cache_with_ttl, ModelWithPricing, ModelsServiceImpl, ModelsServiceTrait,
};
use crate::test_utils::{CapturingUsageService, MockAttestationService};
use config::ExternalProvidersConfig;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Repository whose resolved model can be swapped (or removed) between calls
/// and which counts `resolve_and_get_model` reads.
struct CountingModelsRepository {
    model: Mutex<Option<ModelWithPricing>>,
    resolves: AtomicUsize,
}

impl CountingModelsRepository {
    fn new(model: Option<ModelWithPricing>) -> Arc<Self> {
        Arc::new(Self {
            model: Mutex::new(model),
            resolves: AtomicUsize::new(0),
        })
    }

    fn resolves(&self) -> usize {
        self.resolves.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ModelsRepository for CountingModelsRepository {
    async fn get_all_active_models(&self) -> Result<Vec<ModelWithPricing>, anyhow::Error> {
        Ok(self.model.lock().unwrap().clone().into_iter().collect())
    }

    async fn get_model_by_name(&self, _: &str) -> Result<Option<ModelWithPricing>, anyhow::Error> {
        Ok(self.model.lock().unwrap().clone())
    }

    async fn resolve_and_get_model(
        &self,
        _: &str,
    ) -> Result<Option<ModelWithPricing>, anyhow::Error> {
        self.resolves.fetch_add(1, Ordering::SeqCst);
        Ok(self.model.lock().unwrap().clone())
    }

    async fn get_configured_model_names(&self) -> Result<Vec<String>, anyhow::Error> {
        Ok(vec![])
    }
}

fn pool() -> Arc<InferenceProviderPool> {
    Arc::new(InferenceProviderPool::new(
        None,
        ExternalProvidersConfig::default(),
    ))
}

fn service(repo: Arc<CountingModelsRepository>) -> CompletionServiceImpl {
    CompletionServiceImpl::new(
        pool(),
        Arc::new(MockAttestationService),
        Arc::new(CapturingUsageService::new()),
        Arc::new(CapturingMetricsService::new()),
        repo,
        Arc::new(StaticOrganizationLimitRepository),
    )
}

#[tokio::test]
async fn hit_does_not_call_repository_and_miss_does() {
    let repo = CountingModelsRepository::new(Some(test_model("m")));
    let service = service(repo.clone());

    assert!(service.resolve_model_cached("m").await.unwrap().is_some());
    assert_eq!(repo.resolves(), 1);
    assert!(service.resolve_model_cached("m").await.unwrap().is_some());
    assert_eq!(repo.resolves(), 1);
    assert!(service
        .resolve_model_cached("other")
        .await
        .unwrap()
        .is_some());
    assert_eq!(repo.resolves(), 2);
}

#[tokio::test]
async fn none_is_never_cached() {
    let repo = CountingModelsRepository::new(None);
    let service = service(repo.clone());

    assert!(service.resolve_model_cached("m").await.unwrap().is_none());
    assert!(service.resolve_model_cached("m").await.unwrap().is_none());
    assert_eq!(repo.resolves(), 2);

    // A newly activated model is visible immediately.
    *repo.model.lock().unwrap() = Some(test_model("m"));
    assert!(service.resolve_model_cached("m").await.unwrap().is_some());
}

#[tokio::test]
async fn models_service_invalidation_clears_the_shared_cache() {
    let repo = CountingModelsRepository::new(Some(test_model("m")));
    let models_service = ModelsServiceImpl::new(pool(), repo.clone());
    let service =
        service(repo.clone()).with_model_resolve_cache(models_service.model_resolve_cache());

    service.resolve_model_cached("m").await.unwrap();
    service.resolve_model_cached("m").await.unwrap();
    assert_eq!(repo.resolves(), 1);

    // Admin deactivation: invalidate, then the next request must re-read.
    *repo.model.lock().unwrap() = None;
    models_service.invalidate_models_cache().await;
    assert!(service.resolve_model_cached("m").await.unwrap().is_none());
    assert_eq!(repo.resolves(), 2);
}

#[tokio::test]
async fn entries_expire_after_ttl() {
    let repo = CountingModelsRepository::new(Some(test_model("m")));
    let service = service(repo.clone())
        .with_model_resolve_cache(model_resolve_cache_with_ttl(Duration::from_millis(50)));

    service.resolve_model_cached("m").await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    service.resolve_model_cached("m").await.unwrap();
    assert_eq!(repo.resolves(), 2);
}
