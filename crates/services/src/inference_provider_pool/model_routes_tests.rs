//! The pool's table of model routes (`model_routes`): what it keeps, how it
//! reloads, and that a route never adds a provider to a model's candidates.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use config::ExternalProvidersConfig;
use inference_providers::mock::MockProvider;
use inference_providers::{ChatCompletionParams, CompletionError};
use uuid::Uuid;

use super::model_routes::{build_route_table, MAX_ROUTES_PER_MODEL};
use super::{
    ChatRoutingHints, InferenceProviderPool, InferenceProviderTrait, ModelRoute, ModelRoutesSource,
};
use crate::test_utils::with_test_budget;

const MODEL: &str = "vendor/model";
const TARGET: &str = "vendor/model-via-other";

fn route(model: &str, provider: &str, target: &str) -> ModelRoute {
    ModelRoute {
        id: Uuid::new_v4(),
        model: model.to_string(),
        provider: provider.to_string(),
        target: target.to_string(),
    }
}

fn pool() -> InferenceProviderPool {
    InferenceProviderPool::new(None, ExternalProvidersConfig::default())
}

fn provider() -> Arc<MockProvider> {
    Arc::new(MockProvider::new_accept_all())
}

async fn register(pool: &InferenceProviderPool, model: &str, provider: &Arc<MockProvider>) {
    pool.provider_mappings
        .write()
        .await
        .model_to_providers
        .insert(
            model.to_string(),
            vec![provider.clone() as Arc<InferenceProviderTrait>],
        );
}

fn params(model: &str) -> ChatCompletionParams {
    serde_json::from_value(serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "hello"}]
    }))
    .unwrap()
}

#[test]
fn route_table_groups_by_model_and_drops_the_unusable() {
    let beta = route(MODEL, "beta", "vendor/via-beta");
    let alpha = route(MODEL, "alpha", "vendor/via-alpha");
    let other = route("vendor/other", "alpha", "vendor/via-alpha");
    let table = build_route_table(vec![
        beta.clone(),
        route(MODEL, "self", MODEL),
        alpha.clone(),
        other.clone(),
    ]);

    assert_eq!(table.len(), 2);
    assert_eq!(table[MODEL].to_vec(), vec![alpha, beta]);
    assert_eq!(table["vendor/other"].to_vec(), vec![other]);
}

#[test]
fn route_table_drops_provider_names_callers_cannot_send() {
    let table = build_route_table(
        ["Alpha", "", "-a", "a b", "a\nb", &"a".repeat(33), "a-1"]
            .into_iter()
            .map(|name| route(MODEL, name, "vendor/via"))
            .collect(),
    );
    let kept: Vec<_> = table[MODEL]
        .iter()
        .map(|route| route.provider.as_str())
        .collect();
    assert_eq!(kept, vec!["a-1"]);
}

#[test]
fn route_table_keeps_one_route_per_provider_name() {
    let table = build_route_table(vec![
        route(MODEL, "alpha", "vendor/via-one"),
        route(MODEL, "alpha", "vendor/via-two"),
    ]);
    assert_eq!(table[MODEL].len(), 1);
}

#[test]
fn route_table_caps_the_routes_of_one_model() {
    let routes = (0..MAX_ROUTES_PER_MODEL + 3)
        .map(|index| {
            route(
                MODEL,
                &format!("p{index:02}"),
                &format!("vendor/t{index:02}"),
            )
        })
        .collect();
    let table = build_route_table(routes);
    assert_eq!(table[MODEL].len(), MAX_ROUTES_PER_MODEL);
    assert_eq!(table[MODEL][0].provider, "p00");
}

#[test]
fn the_pool_answers_for_the_routes_of_a_model() {
    let pool = pool();
    assert!(!pool.has_model_routes());
    assert!(pool.model_routes_of(MODEL).is_none());

    let named = route(MODEL, "other", TARGET);
    pool.set_model_routes(vec![named.clone()]);

    assert!(pool.has_model_routes());
    assert_eq!(pool.model_routes_of(MODEL).unwrap().to_vec(), vec![named]);
    assert!(pool.model_routes_of(TARGET).is_none());
}

/// A route is not a fallback: with one in place, the model's candidates are
/// still its own providers only, and a failing own provider fails the
/// request without the target being called.
// Paused clock: the failing provider's retry backoff elapses at once.
#[tokio::test(start_paused = true)]
async fn a_route_adds_no_provider_to_a_models_candidates() {
    let pool = pool();
    let own = provider();
    let other = provider();
    register(&pool, MODEL, &own).await;
    register(&pool, TARGET, &other).await;
    pool.set_model_routes(vec![route(MODEL, "other", TARGET)]);

    let candidates = pool
        .get_providers_with_fallback(MODEL, None, &ChatRoutingHints::default())
        .await
        .expect("registered model");
    assert_eq!(candidates.len(), 1);
    assert!(Arc::ptr_eq(
        &candidates[0],
        &(own.clone() as Arc<InferenceProviderTrait>)
    ));

    own.set_error_override(Some(CompletionError::HttpError {
        status_code: 503,
        message: "upstream unavailable".to_string(),
        is_external: true,
    }))
    .await;
    let result =
        with_test_budget(pool.chat_completion_with_attribution(params(MODEL), "h".into())).await;
    assert!(result.is_err());
    assert_eq!(other.chat_completion_call_count(), 0);
}

/// Yields `routes` on the fetch numbered `answers` (from 0) and an error on
/// every other one.
struct AnsweringOnce {
    routes: Vec<ModelRoute>,
    answers: usize,
    fetches: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelRoutesSource for AnsweringOnce {
    async fn fetch_model_routes(&self) -> Result<Vec<ModelRoute>, String> {
        if self.fetches.fetch_add(1, Ordering::SeqCst) == self.answers {
            Ok(self.routes.clone())
        } else {
            Err("unavailable".to_string())
        }
    }
}

#[tokio::test]
async fn a_failed_reload_keeps_the_last_loaded_routes() {
    let pool = pool();

    // No source: a refresh loads nothing.
    pool.refresh_model_routes().await;
    assert!(!pool.has_model_routes());

    let source = Arc::new(AnsweringOnce {
        routes: vec![route(MODEL, "other", TARGET)],
        answers: 0,
        fetches: AtomicUsize::new(0),
    });
    pool.set_model_routes_source(source.clone());
    pool.refresh_model_routes().await;
    assert!(pool.model_routes_of(MODEL).is_some());

    pool.refresh_model_routes().await;
    assert_eq!(source.fetches.load(Ordering::SeqCst), 2);
    assert!(pool.model_routes_of(MODEL).is_some());
    assert!(!pool.model_routes_unknown());
}

/// Routes are unknown only between setting a source and its first answer: a
/// pool without a source, and one whose source answered with no routes, know
/// that there are none.
#[tokio::test]
async fn routes_are_unknown_until_the_source_first_answers() {
    let without_source = pool();
    without_source.refresh_model_routes().await;
    assert!(!without_source.model_routes_unknown());

    let pool = pool();
    pool.set_model_routes_source(Arc::new(AnsweringOnce {
        routes: Vec::new(),
        answers: 1,
        fetches: AtomicUsize::new(0),
    }));
    assert!(pool.model_routes_unknown());

    pool.refresh_model_routes().await;
    assert!(pool.model_routes_unknown());
    assert!(!pool.has_model_routes());
    assert!(pool.model_routes_of(MODEL).is_none());

    pool.refresh_model_routes().await;
    assert!(!pool.model_routes_unknown());
    assert!(!pool.has_model_routes());
}
