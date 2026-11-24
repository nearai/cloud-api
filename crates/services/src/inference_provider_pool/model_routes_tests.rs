//! Pool behaviour for model routes (`model_routes`): a model whose route
//! targets are other catalog entries.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use config::ExternalProvidersConfig;
use inference_providers::mock::MockProvider;
use inference_providers::{ChatCompletionParams, CompletionError, ProviderTier};
use uuid::Uuid;

use super::model_routes::{build_route_table, MAX_ROUTES_PER_MODEL};
use super::{
    ChatRoutingHints, InferenceProviderPool, InferenceProviderTrait, ModelRoute, ModelRoutesSource,
};
use crate::test_utils::with_test_budget;

const MODEL: &str = "vendor/model";
const TARGET_A: &str = "vendor/model-via-a";
const TARGET_B: &str = "vendor/model-via-b";

fn params(model: &str) -> ChatCompletionParams {
    serde_json::from_value(serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "hello"}]
    }))
    .unwrap()
}

fn provider(tier: ProviderTier) -> Arc<MockProvider> {
    Arc::new(MockProvider::new_accept_all().with_tier(tier))
}

fn external() -> Arc<MockProvider> {
    provider(ProviderTier::NonAttested)
}

fn as_trait(provider: &Arc<MockProvider>) -> Arc<InferenceProviderTrait> {
    provider.clone() as Arc<InferenceProviderTrait>
}

async fn register(pool: &InferenceProviderPool, model: &str, providers: &[&Arc<MockProvider>]) {
    pool.provider_mappings
        .write()
        .await
        .model_to_providers
        .insert(
            model.to_string(),
            providers
                .iter()
                .map(|provider| as_trait(provider))
                .collect(),
        );
}

fn route(model: &str, target: &str, priority: i32) -> ModelRoute {
    ModelRoute {
        id: Uuid::new_v4(),
        model: model.to_string(),
        target: target.to_string(),
        priority,
    }
}

fn pool() -> InferenceProviderPool {
    InferenceProviderPool::new(None, ExternalProvidersConfig::default())
}

fn unavailable() -> CompletionError {
    CompletionError::HttpError {
        status_code: 503,
        message: "upstream unavailable".to_string(),
        is_external: true,
    }
}

/// The candidates of a chat request for `model`, in the order they are tried.
async fn chat_candidates(
    pool: &InferenceProviderPool,
    model: &str,
    hints: &ChatRoutingHints,
) -> Vec<Arc<InferenceProviderTrait>> {
    pool.provider_candidates(model, None, hints, true)
        .await
        .expect("registered model")
        .providers
}

fn same(candidates: &[Arc<InferenceProviderTrait>], expected: &[&Arc<MockProvider>]) -> bool {
    candidates.len() == expected.len()
        && candidates
            .iter()
            .zip(expected)
            .all(|(candidate, expected)| Arc::ptr_eq(candidate, &as_trait(expected)))
}

#[test]
fn route_table_orders_routes_and_drops_the_unusable() {
    let late = route(MODEL, TARGET_A, 50);
    let early = route(MODEL, TARGET_B, 10);
    let table = build_route_table(vec![
        late.clone(),
        route(MODEL, MODEL, 0),
        early.clone(),
        route(MODEL, TARGET_A, 60),
    ]);

    let targets = &table[MODEL];
    assert_eq!(
        targets
            .iter()
            .map(|target| (target.id, target.target.as_str()))
            .collect::<Vec<_>>(),
        vec![(early.id, TARGET_B), (late.id, TARGET_A)],
        "priority order, the self route and the repeated target dropped"
    );
    assert_eq!(table.len(), 1);
}

#[test]
fn route_table_breaks_a_priority_tie_by_target_name() {
    let table = build_route_table(vec![route(MODEL, TARGET_B, 10), route(MODEL, TARGET_A, 10)]);
    let targets: Vec<_> = table[MODEL]
        .iter()
        .map(|target| target.target.as_str())
        .collect();
    assert_eq!(targets, vec![TARGET_A, TARGET_B]);
}

#[test]
fn route_table_caps_the_routes_of_one_model() {
    let routes = (0..MAX_ROUTES_PER_MODEL + 3)
        .map(|index| route(MODEL, &format!("vendor/target-{index:02}"), index as i32))
        .collect();
    let table = build_route_table(routes);
    assert_eq!(table[MODEL].len(), MAX_ROUTES_PER_MODEL);
    assert_eq!(table[MODEL][0].target, "vendor/target-00");
}

#[tokio::test]
async fn a_model_without_routes_keeps_its_candidates() {
    let pool = pool();
    let first = external();
    let second = external();
    let other = external();
    register(&pool, MODEL, &[&first, &second]).await;
    register(&pool, TARGET_A, &[&other]).await;
    // A route of another model does not leak onto this one.
    pool.set_model_routes(vec![route(TARGET_B, TARGET_A, 10)]);

    let hints = ChatRoutingHints {
        prefix_hash: Some(0),
        ..Default::default()
    };
    let with_routes = pool
        .provider_candidates(MODEL, None, &hints, true)
        .await
        .expect("registered model");
    let without_routes = pool
        .get_providers_with_fallback(MODEL, None, &hints)
        .await
        .expect("registered model");

    assert!(with_routes.routes.is_empty());
    assert!(same(&with_routes.providers, &[&first, &second]));
    assert!(same(&without_routes, &[&first, &second]));
}

#[tokio::test]
async fn route_targets_follow_the_models_own_provider_in_route_order() {
    let pool = pool();
    let own = external();
    let via_a = external();
    let via_b = external();
    register(&pool, MODEL, &[&own]).await;
    register(&pool, TARGET_A, &[&via_a]).await;
    register(&pool, TARGET_B, &[&via_b]).await;
    let route_a = route(MODEL, TARGET_A, 20);
    let route_b = route(MODEL, TARGET_B, 10);
    pool.set_model_routes(vec![route_a.clone(), route_b.clone()]);

    let candidates = pool
        .provider_candidates(MODEL, None, &ChatRoutingHints::default(), true)
        .await
        .expect("registered model");

    assert!(same(&candidates.providers, &[&own, &via_b, &via_a]));
    let marks: Vec<_> = candidates.providers[1..]
        .iter()
        .map(|provider| {
            let mark = candidates.routes[&super::model_routes::provider_key(provider)];
            (mark.id, mark.rank)
        })
        .collect();
    assert_eq!(marks, vec![(route_b.id, 1), (route_a.id, 2)]);
}

#[tokio::test]
async fn own_providers_keep_rotating_ahead_of_route_targets() {
    let pool = pool();
    let first = external();
    let second = external();
    let via_a = external();
    register(&pool, MODEL, &[&first, &second]).await;
    register(&pool, TARGET_A, &[&via_a]).await;
    pool.set_model_routes(vec![route(MODEL, TARGET_A, 10)]);

    let hints = ChatRoutingHints::default();
    let one = chat_candidates(&pool, MODEL, &hints).await;
    let two = chat_candidates(&pool, MODEL, &hints).await;

    assert!(same(&one, &[&first, &second, &via_a]));
    assert!(same(&two, &[&second, &first, &via_a]));
}

/// Runs `operation` through the retry loop with a call that fails on `own`
/// and succeeds on any other provider; returns whether a provider served.
async fn served_when_own_fails(
    pool: &InferenceProviderPool,
    operation: &'static str,
    own: &Arc<MockProvider>,
) -> bool {
    let own = as_trait(own);
    pool.retry_with_fallback(MODEL, operation, None, |provider| {
        let own = own.clone();
        async move {
            if Arc::ptr_eq(&provider, &own) {
                Err(unavailable())
            } else {
                Ok(())
            }
        }
    })
    .await
    .is_ok()
}

// Paused clock: the failing provider's retry backoff elapses at once.
#[tokio::test(start_paused = true)]
async fn only_chat_operations_use_routes() {
    let pool = pool();
    let own = external();
    let via_a = external();
    register(&pool, MODEL, &[&own]).await;
    register(&pool, TARGET_A, &[&via_a]).await;
    pool.set_model_routes(vec![route(MODEL, TARGET_A, 10)]);

    for operation in ["chat_completion", "chat_completion_stream"] {
        assert!(
            with_test_budget(served_when_own_fails(&pool, operation, &own)).await,
            "{operation} reaches the route target"
        );
    }
    for operation in ["image_generation", "image_edit", "audio_transcription"] {
        assert!(
            !with_test_budget(served_when_own_fails(&pool, operation, &own)).await,
            "{operation} stays on the model's own provider"
        );
    }
}

#[tokio::test]
async fn the_models_own_provider_serves_while_it_is_healthy() {
    let pool = pool();
    let own = external();
    let via_a = external();
    register(&pool, MODEL, &[&own]).await;
    register(&pool, TARGET_A, &[&via_a]).await;
    pool.set_model_routes(vec![route(MODEL, TARGET_A, 10)]);

    let served = with_test_budget(pool.chat_completion_with_attribution(params(MODEL), "h".into()))
        .await
        .expect("served");

    assert_eq!(own.chat_completion_call_count(), 1);
    assert_eq!(via_a.chat_completion_call_count(), 0);
    assert!(!served.provider_attribution.served_via_fallback);
    assert_eq!(pool.serving_route_id(&served.response.response.id), None);
}

#[tokio::test]
async fn a_route_target_serves_when_the_models_own_provider_fails() {
    let pool = pool();
    let own = external();
    let via_a = external();
    register(&pool, MODEL, &[&own]).await;
    register(&pool, TARGET_A, &[&via_a]).await;
    let route_a = route(MODEL, TARGET_A, 10);
    pool.set_model_routes(vec![route_a.clone()]);
    own.set_error_override(Some(unavailable())).await;

    let served = with_test_budget(pool.chat_completion_with_attribution(params(MODEL), "h".into()))
        .await
        .expect("the route target serves");

    assert_eq!(via_a.chat_completion_call_count(), 1);
    assert!(served.provider_attribution.served_via_fallback);
    assert_eq!(
        served.provider_attribution.served_provider_tier,
        Some(crate::usage::ServedProviderTier::NonAttested)
    );
    let chat_id = served.response.response.id.clone();
    assert_eq!(pool.serving_route_id(&chat_id), Some(route_a.id));

    // The same chat id served by the model's own provider names no route.
    pool.record_chat_route(&chat_id, None);
    assert_eq!(pool.serving_route_id(&chat_id), None);
}

#[tokio::test]
async fn a_route_target_serves_a_stream_when_the_models_own_provider_fails() {
    let pool = pool();
    let own = external();
    let via_a = external();
    register(&pool, MODEL, &[&own]).await;
    register(&pool, TARGET_A, &[&via_a]).await;
    pool.set_model_routes(vec![route(MODEL, TARGET_A, 10)]);
    own.set_error_override(Some(unavailable())).await;

    let served = with_test_budget(pool.chat_completion_stream_with_attribution(
        params(MODEL),
        "h".into(),
        ChatRoutingHints::default(),
    ))
    .await;

    let served = match served {
        Ok(served) => served,
        Err(error) => panic!("the route target serves the stream, got: {error}"),
    };
    assert_eq!(via_a.chat_completion_call_count(), 1);
    assert!(served.provider_attribution.served_via_fallback);
}

#[tokio::test]
async fn timeouts_rate_limits_and_transport_errors_reach_the_route_target() {
    let http = |status_code| CompletionError::HttpError {
        status_code,
        message: "upstream refused".to_string(),
        is_external: true,
    };
    for error in [
        http(408),
        http(429),
        CompletionError::CompletionError("connection refused".to_string()),
        // The provider no longer serves the model at all.
        CompletionError::HttpError {
            status_code: 404,
            message: "The model does not exist".to_string(),
            is_external: true,
        },
    ] {
        let pool = pool();
        let own = external();
        let via_a = external();
        register(&pool, MODEL, &[&own]).await;
        register(&pool, TARGET_A, &[&via_a]).await;
        pool.set_model_routes(vec![route(MODEL, TARGET_A, 10)]);
        own.set_error_override(Some(error.clone())).await;

        let served =
            with_test_budget(pool.chat_completion_with_attribution(params(MODEL), "h".into()))
                .await;

        assert!(served.is_ok(), "{error} should reach the route target");
        assert_eq!(via_a.chat_completion_call_count(), 1);
    }
}

#[tokio::test]
async fn a_request_the_provider_rejects_does_not_reach_a_route_target() {
    let pool = pool();
    let own = external();
    let via_a = external();
    register(&pool, MODEL, &[&own]).await;
    register(&pool, TARGET_A, &[&via_a]).await;
    pool.set_model_routes(vec![route(MODEL, TARGET_A, 10)]);
    own.set_error_override(Some(CompletionError::HttpError {
        status_code: 400,
        message: "bad request".to_string(),
        is_external: true,
    }))
    .await;

    let result =
        with_test_budget(pool.chat_completion_with_attribution(params(MODEL), "h".into())).await;

    assert!(result.is_err());
    assert_eq!(via_a.chat_completion_call_count(), 0);
}

fn set_ttft(pool: &InferenceProviderPool, provider: &Arc<MockProvider>, ttft_ms: f64) {
    let mut states = pool
        .provider_load_state
        .write()
        .unwrap_or_else(|e| e.into_inner());
    let state = states
        .entry(super::model_routes::provider_key(&as_trait(provider)))
        .or_default();
    state.ttft_ewma_ms = ttft_ms;
    state.ttft_samples = super::TTFT_WARMUP_SAMPLES;
}

#[tokio::test]
async fn a_slow_own_provider_still_leads_a_fast_route_target() {
    let pool = pool();
    let fast = external();
    let slow = external();
    let via_a = external();
    register(&pool, MODEL, &[&slow, &fast]).await;
    register(&pool, TARGET_A, &[&via_a]).await;
    pool.set_model_routes(vec![route(MODEL, TARGET_A, 10)]);
    // `slow` counts as slow next to `fast`, so latency orders the model's
    // own providers; the equally fast target still comes last.
    set_ttft(&pool, &fast, 100.0);
    set_ttft(&pool, &slow, 5_000.0);
    set_ttft(&pool, &via_a, 100.0);

    let candidates = chat_candidates(&pool, MODEL, &ChatRoutingHints::default()).await;

    assert!(same(&candidates, &[&fast, &slow, &via_a]));
}

#[tokio::test]
async fn a_fast_route_target_does_not_split_the_models_own_providers() {
    let pool = pool();
    let first = external();
    let second = external();
    let via_a = external();
    register(&pool, MODEL, &[&first, &second]).await;
    register(&pool, TARGET_A, &[&via_a]).await;
    pool.set_model_routes(vec![route(MODEL, TARGET_A, 10)]);
    // Within the slow ratio of each other. Measured against the target,
    // `second` alone would count as slow and stop sharing load with `first`.
    set_ttft(&pool, &first, 400.0);
    set_ttft(&pool, &second, 700.0);
    set_ttft(&pool, &via_a, 100.0);

    let hints = ChatRoutingHints::default();
    let one = chat_candidates(&pool, MODEL, &hints).await;
    let two = chat_candidates(&pool, MODEL, &hints).await;

    assert!(same(&one, &[&first, &second, &via_a]));
    assert!(same(&two, &[&second, &first, &via_a]));
}

#[tokio::test]
async fn a_route_target_leads_once_the_models_own_provider_is_demoted() {
    let pool = pool();
    let own = external();
    let via_a = external();
    register(&pool, MODEL, &[&own]).await;
    register(&pool, TARGET_A, &[&via_a]).await;
    pool.set_model_routes(vec![route(MODEL, TARGET_A, 10)]);
    pool.provider_failure_counts
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(super::model_routes::provider_key(&as_trait(&own)), 10);

    let candidates = chat_candidates(&pool, MODEL, &ChatRoutingHints::default()).await;

    assert!(same(&candidates, &[&via_a, &own]));
}

// Paused clock: the failing provider's retry backoff elapses at once.
#[tokio::test(start_paused = true)]
async fn a_model_with_an_attested_provider_never_gets_route_targets() {
    for tier in [ProviderTier::Near, ProviderTier::Attested3p] {
        let pool = pool();
        let own = provider(tier);
        let via_a = external();
        register(&pool, MODEL, &[&own]).await;
        register(&pool, TARGET_A, &[&via_a]).await;
        pool.set_model_routes(vec![route(MODEL, TARGET_A, 10)]);
        own.set_error_override(Some(unavailable())).await;

        let candidates = chat_candidates(&pool, MODEL, &ChatRoutingHints::default()).await;
        assert!(same(&candidates, &[&own]));

        let result =
            with_test_budget(pool.chat_completion_with_attribution(params(MODEL), "h".into()))
                .await;
        assert!(result.is_err());
        assert_eq!(via_a.chat_completion_call_count(), 0);
    }
}

#[tokio::test]
async fn a_target_with_an_attested_provider_is_skipped() {
    for tier in [ProviderTier::Near, ProviderTier::Attested3p] {
        let pool = pool();
        let own = external();
        let attested = provider(tier);
        let plain = external();
        let via_b = external();
        register(&pool, MODEL, &[&own]).await;
        // Attested alone, and mixed with a non-attested provider.
        register(&pool, TARGET_A, &[&attested, &plain]).await;
        register(&pool, TARGET_B, &[&via_b]).await;
        pool.set_model_routes(vec![route(MODEL, TARGET_A, 10), route(MODEL, TARGET_B, 20)]);

        let candidates = chat_candidates(&pool, MODEL, &ChatRoutingHints::default()).await;

        assert!(same(&candidates, &[&own, &via_b]));
    }
}

#[tokio::test]
async fn a_fallback_opt_out_or_a_pinned_key_gets_no_route_targets() {
    let pool = pool();
    let own = external();
    let via_a = external();
    register(&pool, MODEL, &[&own]).await;
    register(&pool, TARGET_A, &[&via_a]).await;
    pool.set_model_routes(vec![route(MODEL, TARGET_A, 10)]);

    let opted_out = ChatRoutingHints {
        fallback_disabled: true,
        ..Default::default()
    };
    assert!(same(
        &chat_candidates(&pool, MODEL, &opted_out).await,
        &[&own]
    ));

    // The model's own provider carries the pinned key; the request stays on it.
    pool.provider_mappings
        .write()
        .await
        .pubkey_to_providers
        .insert("a-signing-key".to_string(), vec![as_trait(&own)]);
    let pinned = pool
        .provider_candidates(
            MODEL,
            Some("a-signing-key"),
            &ChatRoutingHints::default(),
            true,
        )
        .await
        .expect("the model's own provider carries the key");
    assert!(same(&pinned.providers, &[&own]));
    assert!(pinned.routes.is_empty());
}

#[tokio::test]
async fn routes_are_followed_one_hop_only() {
    let pool = pool();
    let own = external();
    let via_a = external();
    let via_b = external();
    register(&pool, MODEL, &[&own]).await;
    register(&pool, TARGET_A, &[&via_a]).await;
    register(&pool, TARGET_B, &[&via_b]).await;
    pool.set_model_routes(vec![
        route(MODEL, TARGET_A, 10),
        route(TARGET_A, TARGET_B, 10),
    ]);

    let candidates = chat_candidates(&pool, MODEL, &ChatRoutingHints::default()).await;

    assert!(same(&candidates, &[&own, &via_a]));
}

#[tokio::test]
async fn a_target_that_is_not_registered_is_ignored() {
    let pool = pool();
    let own = external();
    let via_b = external();
    register(&pool, MODEL, &[&own]).await;
    register(&pool, TARGET_B, &[&via_b]).await;
    pool.set_model_routes(vec![route(MODEL, TARGET_A, 10), route(MODEL, TARGET_B, 20)]);

    let candidates = chat_candidates(&pool, MODEL, &ChatRoutingHints::default()).await;

    assert!(same(&candidates, &[&own, &via_b]));
}

/// Yields `routes` on the first fetch and an error on every later one.
struct OnceThenFailing {
    routes: Vec<ModelRoute>,
    fetches: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelRoutesSource for OnceThenFailing {
    async fn fetch_model_routes(&self) -> Result<Vec<ModelRoute>, String> {
        if self.fetches.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(self.routes.clone())
        } else {
            Err("unavailable".to_string())
        }
    }
}

#[tokio::test]
async fn a_failed_reload_keeps_the_last_loaded_routes() {
    let pool = pool();
    let own = external();
    let via_a = external();
    register(&pool, MODEL, &[&own]).await;
    register(&pool, TARGET_A, &[&via_a]).await;
    let hints = ChatRoutingHints::default();

    // No source: a refresh loads nothing.
    pool.refresh_model_routes().await;
    assert!(same(&chat_candidates(&pool, MODEL, &hints).await, &[&own]));

    let source = Arc::new(OnceThenFailing {
        routes: vec![route(MODEL, TARGET_A, 10)],
        fetches: AtomicUsize::new(0),
    });
    pool.set_model_routes_source(source.clone());
    pool.refresh_model_routes().await;
    assert!(same(
        &chat_candidates(&pool, MODEL, &hints).await,
        &[&own, &via_a]
    ));

    pool.refresh_model_routes().await;
    assert_eq!(source.fetches.load(Ordering::SeqCst), 2);
    assert!(same(
        &chat_candidates(&pool, MODEL, &hints).await,
        &[&own, &via_a]
    ));
}

#[test]
fn a_chat_id_names_the_route_of_its_latest_chat_only() {
    let pool = pool();
    let (first, second) = (Uuid::new_v4(), Uuid::new_v4());

    pool.record_chat_route("chat", Some(first));
    assert_eq!(pool.serving_route_id("chat"), Some(first));
    pool.record_chat_route("chat", None);
    assert_eq!(pool.serving_route_id("chat"), None);
    pool.record_chat_route("chat", Some(second));
    assert_eq!(pool.serving_route_id("chat"), Some(second));

    // No id, nothing to remember.
    pool.record_chat_route("", Some(first));
    assert_eq!(pool.serving_route_id(""), None);
}

#[test]
fn only_the_most_recent_chat_routes_are_remembered() {
    let pool = pool();
    let route_id = Uuid::new_v4();
    pool.record_chat_route("first", Some(route_id));
    for index in 0..10_000 {
        pool.record_chat_route(&format!("chat-{index}"), Some(route_id));
    }

    assert_eq!(pool.serving_route_id("first"), None);
    assert_eq!(pool.serving_route_id("chat-9999"), Some(route_id));
    assert_eq!(pool.serving_route_id("chat-0"), Some(route_id));
}
