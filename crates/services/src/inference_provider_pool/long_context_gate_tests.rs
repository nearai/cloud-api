//! Pool behaviour for an organization that may not use a model's
//! long-context tier (`ChatRoutingHints::long_context_allowed == false`).

use std::sync::Arc;

use config::ExternalProvidersConfig;
use inference_providers::mock::MockProvider;
use inference_providers::{ChatCompletionParams, CompletionError, ProviderTier};

use super::{ChatRoutingHints, InferenceProviderPool, InferenceProviderTrait};

const MODEL: &str = "org/tiered";
const BASE_CAPACITY: u32 = 1_000;
const LONG_CAPACITY: u32 = 10_000;

fn restricted() -> ChatRoutingHints {
    ChatRoutingHints {
        long_context_allowed: false,
        ..Default::default()
    }
}

/// A chat request whose prompt estimate is about `bytes / 4 * 1.2` tokens.
fn params(model: &str, bytes: usize) -> ChatCompletionParams {
    serde_json::from_value(serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "a".repeat(bytes)}]
    }))
    .unwrap()
}

/// Fits the base tier.
fn small(model: &str) -> ChatCompletionParams {
    params(model, 400)
}

/// About 6_000 tokens: above the base tier, inside the long one.
fn oversized(model: &str) -> ChatCompletionParams {
    params(model, 20_000)
}

fn provider(tier: ProviderTier) -> Arc<MockProvider> {
    Arc::new(MockProvider::new_accept_all().with_tier(tier))
}

/// Register `providers` for `model` with their declared capacities.
async fn register(
    pool: &InferenceProviderPool,
    model: &str,
    providers: &[(&Arc<MockProvider>, Option<u32>)],
) {
    pool.provider_mappings
        .write()
        .await
        .model_to_providers
        .insert(
            model.to_string(),
            providers
                .iter()
                .map(|(provider, _)| (*provider).clone() as Arc<InferenceProviderTrait>)
                .collect(),
        );
    let mut states = pool
        .provider_load_state
        .write()
        .unwrap_or_else(|e| e.into_inner());
    for (provider, capacity) in providers {
        states
            .entry(Arc::as_ptr(*provider) as *const () as usize)
            .or_default()
            .max_context_tokens = *capacity;
    }
}

/// A model with a base tier, a long tier and an attested third-party
/// fallback whose window matches the long tier.
async fn tiered_pool() -> (
    InferenceProviderPool,
    Arc<MockProvider>,
    Arc<MockProvider>,
    Arc<MockProvider>,
) {
    let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
    let base = provider(ProviderTier::Near);
    let long = provider(ProviderTier::Near);
    let fallback = provider(ProviderTier::Attested3p);
    register(
        &pool,
        MODEL,
        &[
            (&base, Some(BASE_CAPACITY)),
            (&long, Some(LONG_CAPACITY)),
            (&fallback, Some(LONG_CAPACITY)),
        ],
    )
    .await;
    (pool, base, long, fallback)
}

fn served_by(result: &Result<impl Sized, CompletionError>) -> Result<(), String> {
    match result {
        Ok(_) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[tokio::test]
async fn oversized_restricted_request_is_rejected_before_any_provider_call() {
    let (pool, base, long, fallback) = tiered_pool().await;

    let non_streaming = pool
        .chat_completion_with_attribution_and_hints(oversized(MODEL), "h1".into(), restricted())
        .await;
    let streaming = pool
        .chat_completion_stream(oversized(MODEL), "h2".into(), restricted())
        .await;

    for error in [non_streaming.err(), streaming.err()] {
        match error {
            Some(CompletionError::HttpError {
                status_code,
                message,
                is_external,
            }) => {
                assert_eq!(status_code, 400);
                assert!(!is_external);
                assert_eq!(message, super::long_context_limit_message(BASE_CAPACITY));
                assert!(message.contains("limit: 1000 tokens"), "{message}");
            }
            Some(other) => panic!("expected a 400, got: {other}"),
            None => panic!("an oversized restricted request must be rejected"),
        }
    }
    for provider in [&base, &long, &fallback] {
        assert_eq!(provider.chat_completion_call_count(), 0);
    }

    // The typed pre-check agrees.
    assert_eq!(
        pool.long_context_limit_exceeded(&oversized(MODEL), false)
            .await,
        Some(BASE_CAPACITY)
    );
    assert_eq!(
        pool.long_context_limit_exceeded(&oversized(MODEL), true)
            .await,
        None
    );
    assert_eq!(
        pool.long_context_limit_exceeded(&small(MODEL), false).await,
        None
    );
}

#[tokio::test]
async fn oversized_unrestricted_request_is_served_by_the_long_tier() {
    let (pool, base, long, _) = tiered_pool().await;
    let result = pool
        .chat_completion_with_attribution_and_hints(
            oversized(MODEL),
            "h".into(),
            ChatRoutingHints::default(),
        )
        .await;
    served_by(&result).unwrap();
    assert_eq!(long.chat_completion_call_count(), 1);
    assert_eq!(base.chat_completion_call_count(), 0);
}

/// A retryable error on the base tier fails over to the long tier for an
/// unrestricted organization, never for a restricted one; the third-party
/// fallback still serves both.
#[tokio::test(start_paused = true)]
async fn retryable_base_error_never_reaches_the_long_tier_when_restricted() {
    for (hints, long_may_serve) in [(restricted(), false), (ChatRoutingHints::default(), true)] {
        let (pool, base, long, fallback) = tiered_pool().await;
        base.set_error_override(Some(CompletionError::HttpError {
            status_code: 503,
            message: "queue full".to_string(),
            is_external: false,
        }))
        .await;
        // Without a fallback the restricted request has nowhere else to go.
        fallback
            .set_error_override(Some(CompletionError::HttpError {
                status_code: 503,
                message: "queue full".to_string(),
                is_external: true,
            }))
            .await;

        let result = pool
            .chat_completion_with_attribution_and_hints(small(MODEL), "h".into(), hints)
            .await;

        assert!(base.chat_completion_call_count() > 0);
        if long_may_serve {
            served_by(&result).unwrap();
            assert!(long.chat_completion_call_count() > 0);
        } else {
            assert!(matches!(
                result,
                Err(CompletionError::HttpError {
                    status_code: 503,
                    ..
                })
            ));
            assert_eq!(
                long.chat_completion_call_count(),
                0,
                "a restricted request must never reach the long tier, also not on retry"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn retryable_base_error_falls_back_to_the_third_party_when_restricted() {
    let (pool, base, long, fallback) = tiered_pool().await;
    base.set_error_override(Some(CompletionError::HttpError {
        status_code: 503,
        message: "queue full".to_string(),
        is_external: false,
    }))
    .await;

    let result = pool
        .chat_completion_with_attribution_and_hints(small(MODEL), "h".into(), restricted())
        .await;

    served_by(&result).unwrap();
    assert_eq!(fallback.chat_completion_call_count(), 1);
    assert_eq!(long.chat_completion_call_count(), 0);
}

/// A context-length 400 from the base tier falls through to the larger long
/// tier for an unrestricted organization only.
#[tokio::test]
async fn context_length_400_never_falls_through_to_the_long_tier_when_restricted() {
    for (hints, long_may_serve) in [(restricted(), false), (ChatRoutingHints::default(), true)] {
        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let base = provider(ProviderTier::Near);
        let long = provider(ProviderTier::Near);
        register(
            &pool,
            MODEL,
            &[(&base, Some(BASE_CAPACITY)), (&long, Some(LONG_CAPACITY))],
        )
        .await;
        base.set_error_override(Some(CompletionError::HttpError {
            status_code: 400,
            message: "This model's maximum context length is 1000 tokens.".to_string(),
            is_external: false,
        }))
        .await;

        let streaming = pool
            .chat_completion_stream(small(MODEL), "h".into(), hints)
            .await;

        assert_eq!(base.chat_completion_call_count(), 1);
        if long_may_serve {
            served_by(&streaming).unwrap();
            assert_eq!(long.chat_completion_call_count(), 1);
        } else {
            assert!(matches!(
                streaming,
                Err(CompletionError::HttpError {
                    status_code: 400,
                    ..
                })
            ));
            assert_eq!(long.chat_completion_call_count(), 0);
        }
    }
}

#[tokio::test]
async fn restricted_flag_does_not_change_a_single_tier_model() {
    let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
    let near = provider(ProviderTier::Near);
    // A fallback with a smaller window must not make the NEAR fleet look like
    // a long tier.
    let fallback = provider(ProviderTier::Attested3p);
    register(
        &pool,
        MODEL,
        &[
            (&near, Some(LONG_CAPACITY)),
            (&fallback, Some(BASE_CAPACITY)),
        ],
    )
    .await;

    assert_eq!(pool.long_context_base_capacity(MODEL).await, None);
    assert_eq!(
        pool.long_context_limit_exceeded(&oversized(MODEL), false)
            .await,
        None
    );

    let restricted_order = pool
        .get_providers_with_fallback(MODEL, None, &restricted())
        .await
        .unwrap();
    assert_eq!(restricted_order.len(), 2);

    let result = pool
        .chat_completion_with_attribution_and_hints(oversized(MODEL), "h".into(), restricted())
        .await;
    served_by(&result).unwrap();
    assert_eq!(near.chat_completion_call_count(), 1);
}

#[tokio::test]
async fn restricted_candidates_exclude_only_the_long_tier() {
    let (pool, base, long, fallback) = tiered_pool().await;
    assert_eq!(
        pool.long_context_base_capacity(MODEL).await,
        Some(BASE_CAPACITY)
    );

    let ptr = |provider: &Arc<InferenceProviderTrait>| Arc::as_ptr(provider) as *const () as usize;
    let mock_ptr = |provider: &Arc<MockProvider>| Arc::as_ptr(provider) as *const () as usize;

    let restricted_order: Vec<usize> = pool
        .get_providers_with_fallback(MODEL, None, &restricted())
        .await
        .unwrap()
        .iter()
        .map(ptr)
        .collect();
    assert_eq!(restricted_order.len(), 2);
    assert!(restricted_order.contains(&mock_ptr(&base)));
    assert!(restricted_order.contains(&mock_ptr(&fallback)));
    assert!(!restricted_order.contains(&mock_ptr(&long)));

    let unrestricted = pool
        .get_providers_with_fallback(MODEL, None, &ChatRoutingHints::default())
        .await
        .unwrap();
    assert_eq!(unrestricted.len(), 3);
}

#[tokio::test]
async fn a_signing_key_of_the_long_tier_finds_no_provider_when_restricted() {
    let (pool, _base, long, _fallback) = tiered_pool().await;
    let key = "long-tier-signing-key";
    pool.provider_mappings
        .write()
        .await
        .pubkey_to_providers
        .insert(
            key.to_string(),
            vec![long.clone() as Arc<InferenceProviderTrait>],
        );

    assert!(pool
        .get_providers_with_fallback(MODEL, Some(key), &restricted())
        .await
        .is_none());
    assert_eq!(
        pool.get_providers_with_fallback(MODEL, Some(key), &ChatRoutingHints::default())
            .await
            .map(|providers| providers.len()),
        Some(1)
    );
}

#[tokio::test]
async fn restricted_attestation_report_leaves_out_the_long_tier() {
    let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
    let fleet = |name: &'static str| {
        Arc::new(
            MockProvider::new_accept_all()
                .with_tier(ProviderTier::Near)
                .with_attestation_reports_handler(move |_, _, _, _, _| {
                    let mut report = serde_json::Map::new();
                    report.insert("fleet".to_string(), serde_json::json!(name));
                    Ok(vec![report])
                }),
        )
    };
    let (base, long) = (fleet("base"), fleet("long"));
    register(
        &pool,
        MODEL,
        &[(&base, Some(BASE_CAPACITY)), (&long, Some(LONG_CAPACITY))],
    )
    .await;

    let fleets = |reports: Vec<serde_json::Map<String, serde_json::Value>>| -> Vec<String> {
        reports
            .iter()
            .map(|report| report["fleet"].as_str().unwrap().to_string())
            .collect()
    };
    let restricted = pool
        .get_attestation_report(MODEL.into(), None, None, None, false, None, false)
        .await
        .unwrap();
    assert_eq!(fleets(restricted), vec!["base"]);
    let full = pool
        .get_attestation_report(MODEL.into(), None, None, None, false, None, true)
        .await
        .unwrap();
    assert_eq!(fleets(full), vec!["base", "long"]);
}

/// System One builds its own hints: a restricted call never reaches the long
/// tier, also when the base tier fails with a retryable error.
#[tokio::test(start_paused = true)]
async fn systemone_never_reaches_the_long_tier_when_restricted() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    for (long_context_allowed, long_may_be_called) in [(false, false), (true, true)] {
        let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
        let tier = |calls: Arc<AtomicUsize>| {
            Arc::new(
                MockProvider::new_accept_all()
                    .with_tier(ProviderTier::Near)
                    .with_systemone_handler(move |_, _| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Err(CompletionError::HttpError {
                            status_code: 503,
                            message: "queue full".to_string(),
                            is_external: false,
                        })
                    }),
            )
        };
        let (base_calls, long_calls) =
            (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (base, long) = (tier(base_calls.clone()), tier(long_calls.clone()));
        register(
            &pool,
            MODEL,
            &[(&base, Some(BASE_CAPACITY)), (&long, Some(LONG_CAPACITY))],
        )
        .await;
        let request = serde_json::from_value(serde_json::json!({
            "model": MODEL,
            "state": {"message": "m"},
            "questions": {"q": {"type": "noul", "instructions": "i"}}
        }))
        .unwrap();

        let result = pool
            .systemone_with_attribution(request, "h".into(), None, false, long_context_allowed)
            .await;

        assert!(result.is_err());
        assert!(base_calls.load(Ordering::SeqCst) > 0);
        assert_eq!(
            long_calls.load(Ordering::SeqCst) > 0,
            long_may_be_called,
            "long_context_allowed: {long_context_allowed}"
        );
    }
}
