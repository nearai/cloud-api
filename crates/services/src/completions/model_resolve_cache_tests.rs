use super::provider_attribution_tests::{
    completion_request, test_model, StaticOrganizationLimitRepository,
};
use super::*;
use crate::inference_provider_pool::InferenceProviderPool;
use crate::metrics::capturing::CapturingMetricsService;
use crate::models::{
    model_resolve_cache_with_ttl, ModelCaller, ModelWithPricing, ModelsServiceImpl,
    ModelsServiceTrait,
};
use crate::test_utils::{CapturingUsageService, MockAttestationService};
use config::ExternalProvidersConfig;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Repository whose resolved model can be swapped (or removed) between calls
/// and which counts `resolve_and_get_model` reads. It also holds the grants
/// on that model and counts `organization_has_model_access` reads.
struct CountingModelsRepository {
    model: Mutex<Option<ModelWithPricing>>,
    resolves: AtomicUsize,
    delay: Duration,
    fail: std::sync::atomic::AtomicBool,
    granted_organizations: Mutex<HashSet<Uuid>>,
    access_lookups: AtomicUsize,
}

impl CountingModelsRepository {
    fn new(model: Option<ModelWithPricing>) -> Arc<Self> {
        Self::slow(model, Duration::ZERO)
    }

    fn slow(model: Option<ModelWithPricing>, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            model: Mutex::new(model),
            resolves: AtomicUsize::new(0),
            delay,
            fail: std::sync::atomic::AtomicBool::new(false),
            granted_organizations: Mutex::new(HashSet::new()),
            access_lookups: AtomicUsize::new(0),
        })
    }

    /// A repository holding one unlisted model named `m`.
    fn unlisted() -> Arc<Self> {
        let mut model = test_model("m");
        model.is_listed = false;
        Self::new(Some(model))
    }

    fn resolves(&self) -> usize {
        self.resolves.load(Ordering::SeqCst)
    }

    fn access_lookups(&self) -> usize {
        self.access_lookups.load(Ordering::SeqCst)
    }

    fn grant(&self, organization_id: Uuid) {
        self.granted_organizations
            .lock()
            .unwrap()
            .insert(organization_id);
    }

    fn revoke(&self, organization_id: Uuid) {
        self.granted_organizations
            .lock()
            .unwrap()
            .remove(&organization_id);
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
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        if self.fail.load(Ordering::SeqCst) {
            anyhow::bail!("db down");
        }
        Ok(self.model.lock().unwrap().clone())
    }

    async fn get_configured_model_names(&self) -> Result<Vec<String>, anyhow::Error> {
        Ok(vec![])
    }

    async fn organization_has_model_access(
        &self,
        organization_id: Uuid,
        _model_id: Uuid,
    ) -> Result<bool, anyhow::Error> {
        self.access_lookups.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .granted_organizations
            .lock()
            .unwrap()
            .contains(&organization_id))
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

    assert!(service
        .resolve_model_cached("m", ModelCaller::Trusted)
        .await
        .unwrap()
        .is_some());
    assert_eq!(repo.resolves(), 1);
    assert!(service
        .resolve_model_cached("m", ModelCaller::Trusted)
        .await
        .unwrap()
        .is_some());
    assert_eq!(repo.resolves(), 1);
    assert!(service
        .resolve_model_cached("other", ModelCaller::Trusted)
        .await
        .unwrap()
        .is_some());
    assert_eq!(repo.resolves(), 2);
}

#[tokio::test]
async fn none_is_never_cached() {
    let repo = CountingModelsRepository::new(None);
    let service = service(repo.clone());

    assert!(service
        .resolve_model_cached("m", ModelCaller::Trusted)
        .await
        .unwrap()
        .is_none());
    assert!(service
        .resolve_model_cached("m", ModelCaller::Trusted)
        .await
        .unwrap()
        .is_none());
    assert_eq!(repo.resolves(), 2);

    // A newly activated model is visible immediately.
    *repo.model.lock().unwrap() = Some(test_model("m"));
    assert!(service
        .resolve_model_cached("m", ModelCaller::Trusted)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn models_service_invalidation_clears_the_shared_cache() {
    let repo = CountingModelsRepository::new(Some(test_model("m")));
    let models_service = ModelsServiceImpl::new(pool(), repo.clone());
    let service =
        service(repo.clone()).with_model_resolve_cache(models_service.model_resolve_cache());

    service
        .resolve_model_cached("m", ModelCaller::Trusted)
        .await
        .unwrap();
    service
        .resolve_model_cached("m", ModelCaller::Trusted)
        .await
        .unwrap();
    assert_eq!(repo.resolves(), 1);

    // Admin deactivation: invalidate, then the next request must re-read.
    *repo.model.lock().unwrap() = None;
    models_service.invalidate_models_cache().await;
    assert!(service
        .resolve_model_cached("m", ModelCaller::Trusted)
        .await
        .unwrap()
        .is_none());
    assert_eq!(repo.resolves(), 2);
}

#[tokio::test]
async fn entries_expire_after_ttl() {
    let repo = CountingModelsRepository::new(Some(test_model("m")));
    let service = service(repo.clone())
        .with_model_resolve_cache(model_resolve_cache_with_ttl(Duration::from_millis(50)));

    service
        .resolve_model_cached("m", ModelCaller::Trusted)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    service
        .resolve_model_cached("m", ModelCaller::Trusted)
        .await
        .unwrap();
    assert_eq!(repo.resolves(), 2);
}

#[tokio::test]
async fn concurrent_misses_for_one_key_read_the_repository_once() {
    let repo = CountingModelsRepository::slow(Some(test_model("m")), Duration::from_millis(100));
    let service = Arc::new(service(repo.clone()));

    let tasks: Vec<_> = (0..16)
        .map(|_| {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .resolve_model_cached("m", ModelCaller::Trusted)
                    .await
            })
        })
        .collect();
    for task in tasks {
        assert!(task.await.unwrap().unwrap().is_some());
    }
    assert_eq!(repo.resolves(), 1);
}

#[tokio::test]
async fn concurrent_none_misses_are_coalesced_but_not_cached() {
    let repo = CountingModelsRepository::slow(None, Duration::from_millis(100));
    let service = Arc::new(service(repo.clone()));

    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .resolve_model_cached("m", ModelCaller::Trusted)
                    .await
            })
        })
        .collect();
    for task in tasks {
        assert!(task.await.unwrap().unwrap().is_none());
    }
    assert_eq!(repo.resolves(), 1);

    // Not cached: the next request re-reads and sees a newly activated model.
    *repo.model.lock().unwrap() = Some(test_model("m"));
    assert!(service
        .resolve_model_cached("m", ModelCaller::Trusted)
        .await
        .unwrap()
        .is_some());
    assert_eq!(repo.resolves(), 2);
}

#[tokio::test]
async fn repository_errors_propagate_to_coalesced_callers_and_are_not_cached() {
    let repo = CountingModelsRepository::slow(Some(test_model("m")), Duration::from_millis(100));
    repo.fail.store(true, Ordering::SeqCst);
    let service = Arc::new(service(repo.clone()));

    let tasks: Vec<_> = (0..4)
        .map(|_| {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .resolve_model_cached("m", ModelCaller::Trusted)
                    .await
            })
        })
        .collect();
    for task in tasks {
        assert!(task.await.unwrap().is_err());
    }
    assert_eq!(repo.resolves(), 1);

    repo.fail.store(false, Ordering::SeqCst);
    assert!(service
        .resolve_model_cached("m", ModelCaller::Trusted)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn a_listed_model_resolves_for_any_organization_without_a_grant_lookup() {
    let repo = CountingModelsRepository::new(Some(test_model("m")));
    let service = service(repo.clone());

    for _ in 0..3 {
        let caller = ModelCaller::Organization(Uuid::new_v4());
        assert!(service
            .resolve_model_cached("m", caller)
            .await
            .unwrap()
            .is_some());
    }
    assert_eq!(repo.resolves(), 1);
    assert_eq!(repo.access_lookups(), 0);
}

#[tokio::test]
async fn an_unlisted_model_resolves_only_for_an_organization_that_holds_a_grant() {
    let repo = CountingModelsRepository::unlisted();
    let service = service(repo.clone());
    let granted = Uuid::new_v4();
    repo.grant(granted);

    assert!(service
        .resolve_model_cached("m", ModelCaller::Organization(granted))
        .await
        .unwrap()
        .is_some());
    for caller in [
        ModelCaller::Organization(Uuid::new_v4()),
        ModelCaller::Anonymous,
    ] {
        assert!(service
            .resolve_model_cached("m", caller)
            .await
            .unwrap()
            .is_none());
        // The lookup the Responses API makes before dispatch sees the same.
        assert!(service.get_model("m", caller).await.unwrap().is_none());
    }
    // One read of the model serves every caller; only organizations reach
    // the grants.
    assert_eq!(repo.resolves(), 1);
    assert_eq!(repo.access_lookups(), 2);
}

#[tokio::test]
async fn models_service_invalidation_clears_the_shared_access_decisions() {
    let repo = CountingModelsRepository::unlisted();
    let models_service = ModelsServiceImpl::new(pool(), repo.clone());
    let service =
        service(repo.clone()).with_model_resolve_cache(models_service.model_resolve_cache());
    let organization = Uuid::new_v4();
    let caller = ModelCaller::Organization(organization);

    // A refusal is cached.
    assert!(service
        .resolve_model_cached("m", caller)
        .await
        .unwrap()
        .is_none());
    repo.grant(organization);
    assert!(service
        .resolve_model_cached("m", caller)
        .await
        .unwrap()
        .is_none());
    assert_eq!(repo.access_lookups(), 1);

    // Granting through the admin API invalidates: the grant applies at once.
    models_service.invalidate_models_cache().await;
    assert!(service
        .resolve_model_cached("m", caller)
        .await
        .unwrap()
        .is_some());

    // So does revoking.
    repo.revoke(organization);
    models_service.invalidate_models_cache().await;
    assert!(service
        .resolve_model_cached("m", caller)
        .await
        .unwrap()
        .is_none());
    assert_eq!(repo.access_lookups(), 3);
}

/// The completion service must answer a model the organization may not use
/// with the error it gives for a name that does not exist.
#[tokio::test]
async fn a_refused_model_gets_the_completion_error_of_an_unknown_one() {
    let repo = CountingModelsRepository::unlisted();
    let service = service(repo.clone());

    let error_for = |model: &'static str| {
        let service = &service;
        async move {
            let Err(blocking) = service
                .create_chat_completion(completion_request(model))
                .await
            else {
                panic!("the request must be refused");
            };
            let Err(streaming) = service
                .create_chat_completion_stream(completion_request(model))
                .await
            else {
                panic!("the streaming request must be refused");
            };
            (blocking, streaming)
        }
    };

    let (refused, refused_stream) = error_for("m").await;
    // The repository holds one model, so remove it to make `m` unknown.
    *repo.model.lock().unwrap() = None;
    let (unknown, unknown_stream) = error_for("m").await;

    for (refused, unknown) in [(refused, unknown), (refused_stream, unknown_stream)] {
        assert!(matches!(refused, CompletionError::InvalidModel(_)));
        assert!(matches!(unknown, CompletionError::InvalidModel(_)));
        assert_eq!(refused.to_string(), unknown.to_string());
    }
}

#[tokio::test]
async fn a_granted_organization_is_served_an_unlisted_model() {
    let repo = CountingModelsRepository::unlisted();
    let pool = pool();
    pool.register_provider(
        "m".to_string(),
        Arc::new(inference_providers::mock::MockProvider::new_accept_all()),
    )
    .await;
    let service = CompletionServiceImpl::new(
        pool,
        Arc::new(MockAttestationService),
        Arc::new(CapturingUsageService::new()),
        Arc::new(CapturingMetricsService::new()),
        repo.clone(),
        Arc::new(StaticOrganizationLimitRepository),
    );

    // The provider is there, yet an organization without a grant is refused.
    let Err(refused) = service
        .create_chat_completion(completion_request("m"))
        .await
    else {
        panic!("an organization without a grant must be refused");
    };
    assert_eq!(
        refused.to_string(),
        "Invalid model: Model 'm' not found. It's not a valid model name or alias."
    );

    let request = completion_request("m");
    repo.grant(request.organization_id);
    service
        .create_chat_completion(request)
        .await
        .expect("the granted organization is served");
}
