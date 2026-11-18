//! The completion service applies an organization's long-context access:
//! a typed error for a request above the limit, and the routing flag for the
//! rest.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use inference_providers::mock::MockProvider;
use inference_providers::{CompletionError as ProviderError, ProviderTier};

use super::provider_attribution_tests::{
    completion_request, test_model, StaticModelsRepository, StaticOrganizationLimitRepository,
};
use super::*;
use crate::inference_provider_pool::ProviderPoolRole;
use crate::long_context_access::{LongContextAccess, LongContextAccessRepository};
use crate::metrics::capturing::CapturingMetricsService;
use crate::test_utils::{CapturingUsageService, MockAttestationService};

const MODEL: &str = "org/tiered";

/// Blocks `blocked` from every model's long tier; counts reads.
struct Blocks {
    blocked: Uuid,
    reads: AtomicUsize,
}

#[async_trait::async_trait]
impl LongContextAccessRepository for Blocks {
    async fn blocked_model_names(&self, organization_id: Uuid) -> anyhow::Result<Vec<String>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(if organization_id == self.blocked {
            vec![MODEL.to_string()]
        } else {
            Vec::new()
        })
    }

    async fn organization_settings(&self, _: Uuid) -> anyhow::Result<Option<serde_json::Value>> {
        Ok(None)
    }
}

struct Fixture {
    service: CompletionServiceImpl,
    base: Arc<MockProvider>,
    long: Arc<MockProvider>,
    blocks: Arc<Blocks>,
    blocked: Uuid,
}

/// `MODEL` with a base tier of 1_000 tokens and a long tier of 10_000 (or a
/// single 10_000-token tier when `tiered` is false).
async fn fixture(tiered: bool) -> Fixture {
    let pool = Arc::new(InferenceProviderPool::new(
        None,
        config::ExternalProvidersConfig::default(),
    ));
    let base = Arc::new(MockProvider::new_accept_all().with_tier(ProviderTier::Near));
    let long = Arc::new(MockProvider::new_accept_all().with_tier(ProviderTier::Near));
    let mut fleets = vec![(long.clone(), 10_000)];
    if tiered {
        fleets.push((base.clone(), 1_000));
        // The catalog row's configured tiers, as an inference-URL load records.
        pool.record_long_context_boundaries(&[
            (
                MODEL.to_string(),
                "https://base.example".to_string(),
                Some(1_000),
            ),
            (
                MODEL.to_string(),
                "https://long.example".to_string(),
                Some(10_000),
            ),
        ]);
    }
    for (provider, capacity) in fleets {
        pool.register_pinned_provider(
            MODEL.to_string(),
            provider,
            Some(capacity),
            ProviderPoolRole::Primary,
        )
        .await;
    }

    let blocked = Uuid::new_v4();
    let blocks = Arc::new(Blocks {
        blocked,
        reads: AtomicUsize::new(0),
    });
    let access = Arc::new(LongContextAccess::new(blocks.clone()));

    let service = CompletionServiceImpl::new(
        pool,
        Arc::new(MockAttestationService),
        Arc::new(CapturingUsageService::new()),
        Arc::new(CapturingMetricsService::new()),
        Arc::new(StaticModelsRepository {
            model: test_model(MODEL),
        }),
        Arc::new(StaticOrganizationLimitRepository),
    )
    .with_long_context_access(access);
    Fixture {
        service,
        base,
        long,
        blocks,
        blocked,
    }
}

fn request(organization_id: Uuid, bytes: usize) -> ports::CompletionRequest {
    let mut request = completion_request(MODEL);
    request.organization_id = organization_id;
    request.messages[0].content = serde_json::Value::String("a".repeat(bytes));
    request
}

#[tokio::test]
async fn oversized_request_of_a_blocked_organization_gets_the_typed_error() {
    let f = fixture(true).await;

    let non_streaming = f
        .service
        .create_chat_completion(request(f.blocked, 20_000))
        .await
        .err();
    let streaming = f
        .service
        .create_chat_completion_stream(request(f.blocked, 20_000))
        .await
        .err();

    for error in [non_streaming, streaming] {
        match error {
            Some(ports::CompletionError::ContextLengthExceeded { limit }) => {
                assert_eq!(limit, 1_000)
            }
            Some(other) => panic!("expected ContextLengthExceeded, got: {other}"),
            None => panic!("the request must be rejected"),
        }
    }
    assert_eq!(f.base.chat_completion_call_count(), 0);
    assert_eq!(f.long.chat_completion_call_count(), 0);
    assert_eq!(
        ports::CompletionError::ContextLengthExceeded { limit: 1_000 }.to_string(),
        "This request needs more context than this model allows for your organization \
         (limit: 1000 tokens)."
    );
}

#[tokio::test]
async fn oversized_request_of_another_organization_reaches_the_long_tier() {
    let f = fixture(true).await;
    f.service
        .create_chat_completion(request(Uuid::new_v4(), 20_000))
        .await
        .expect("served by the long tier");
    assert_eq!(f.long.chat_completion_call_count(), 1);
    assert_eq!(f.base.chat_completion_call_count(), 0);
}

/// The flag reaches the pool: a retryable error on the base tier does not
/// fail over to the long tier for the blocked organization.
#[tokio::test(start_paused = true)]
async fn small_request_of_a_blocked_organization_never_reaches_the_long_tier() {
    let f = fixture(true).await;
    f.base
        .set_error_override(Some(ProviderError::HttpError {
            status_code: 503,
            message: "queue full".to_string(),
            is_external: false,
        }))
        .await;

    assert!(f
        .service
        .create_chat_completion(request(f.blocked, 400))
        .await
        .is_err());
    assert!(f.base.chat_completion_call_count() > 0);
    assert_eq!(f.long.chat_completion_call_count(), 0);

    // Control: another organization fails over to the long tier.
    f.service
        .create_chat_completion(request(Uuid::new_v4(), 400))
        .await
        .expect("failover to the long tier");
    assert!(f.long.chat_completion_call_count() > 0);
}

#[tokio::test]
async fn a_single_tier_model_needs_no_access_lookup() {
    let f = fixture(false).await;
    assert!(
        ports::CompletionServiceTrait::long_context_allowed(&f.service, f.blocked, MODEL).await
    );
    f.service
        .create_chat_completion(request(f.blocked, 20_000))
        .await
        .expect("a single-tier model is unaffected");
    assert_eq!(f.blocks.reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn long_context_allowed_reports_the_organization_access() {
    let f = fixture(true).await;
    assert!(
        !ports::CompletionServiceTrait::long_context_allowed(&f.service, f.blocked, MODEL).await
    );
    assert!(
        ports::CompletionServiceTrait::long_context_allowed(&f.service, Uuid::new_v4(), MODEL)
            .await
    );
}

/// Tool definitions typed parsing cannot read stay in `extra` and are
/// forwarded; the pre-dispatch check counts them like dispatch does.
#[tokio::test]
async fn the_precheck_counts_tool_definitions_left_in_extra() {
    let f = fixture(true).await;
    let mut request = request(f.blocked, 40);
    // An unknown tool type: typed parsing falls back and keeps the raw list.
    request.extra.insert(
        "tools".to_string(),
        serde_json::json!([{"type": "custom_search", "description": "d".repeat(20_000)}]),
    );
    match ports::CompletionServiceTrait::check_long_context_limit(&f.service, &request).await {
        Err(ports::CompletionError::ContextLengthExceeded { limit }) => assert_eq!(limit, 1_000),
        other => panic!("expected ContextLengthExceeded, got: {other:?}"),
    }
    // Without the tools the same request fits.
    request.extra.remove("tools");
    assert!(
        ports::CompletionServiceTrait::check_long_context_limit(&f.service, &request)
            .await
            .is_ok()
    );
    assert_eq!(f.base.chat_completion_call_count(), 0);
    assert_eq!(f.long.chat_completion_call_count(), 0);
}

/// Model catalog whose lookups fail.
struct FailingModelsRepository;

#[async_trait::async_trait]
impl crate::models::ModelsRepository for FailingModelsRepository {
    async fn get_all_active_models(
        &self,
    ) -> Result<Vec<crate::models::ModelWithPricing>, anyhow::Error> {
        anyhow::bail!("catalog unavailable")
    }

    async fn get_model_by_name(
        &self,
        _: &str,
    ) -> Result<Option<crate::models::ModelWithPricing>, anyhow::Error> {
        anyhow::bail!("catalog unavailable")
    }

    async fn resolve_and_get_model(
        &self,
        _: &str,
    ) -> Result<Option<crate::models::ModelWithPricing>, anyhow::Error> {
        anyhow::bail!("catalog unavailable")
    }

    async fn get_configured_model_names(&self) -> Result<Vec<String>, anyhow::Error> {
        anyhow::bail!("catalog unavailable")
    }
}

/// The pre-dispatch check fails on an unknown model or a failed model lookup
/// (with dispatch's errors) instead of letting the caller's paid work run.
#[tokio::test]
async fn the_precheck_fails_on_an_unknown_model_or_a_failed_lookup() {
    let f = fixture(true).await;
    let mut unknown = request(f.blocked, 40);
    unknown.model = "org/unknown".to_string();
    assert!(matches!(
        ports::CompletionServiceTrait::check_long_context_limit(&f.service, &unknown).await,
        Err(ports::CompletionError::InvalidModel(_))
    ));

    let failing = CompletionServiceImpl::new(
        Arc::new(InferenceProviderPool::new(
            None,
            config::ExternalProvidersConfig::default(),
        )),
        Arc::new(MockAttestationService),
        Arc::new(CapturingUsageService::new()),
        Arc::new(CapturingMetricsService::new()),
        Arc::new(FailingModelsRepository),
        Arc::new(StaticOrganizationLimitRepository),
    );
    assert!(matches!(
        ports::CompletionServiceTrait::check_long_context_limit(
            &failing,
            &request(Uuid::new_v4(), 40)
        )
        .await,
        Err(ports::CompletionError::InternalError(_))
    ));
}
