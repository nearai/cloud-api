//! Retry and fallback behaviour of `InferenceProviderPool::privacy_classify`
//! for upstream overload (503) and timeout (504) answers.

use super::*;
use inference_providers::mock::MockProvider;
use inference_providers::{PrivacyClassifyError, ProviderTier};
use std::sync::atomic::{AtomicUsize, Ordering};

const MODEL: &str = "test/privacy-filter";

type Outcome = fn() -> Result<bytes::Bytes, PrivacyClassifyError>;

/// A mock provider at `tier` that answers every classify call with
/// `outcome` and counts the calls.
fn counting_provider(
    tier: ProviderTier,
    outcome: Outcome,
) -> (Arc<InferenceProviderTrait>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let provider = MockProvider::new()
        .with_tier(tier)
        .with_privacy_classify_handler(move |_body| {
            counter.fetch_add(1, Ordering::SeqCst);
            outcome()
        });
    (Arc::new(provider), calls)
}

async fn pool_with(providers: Vec<Arc<InferenceProviderTrait>>) -> InferenceProviderPool {
    let pool = InferenceProviderPool::new(None, ExternalProvidersConfig::default());
    pool.provider_mappings
        .write()
        .await
        .model_to_providers
        .insert(MODEL.to_string(), providers);
    pool
}

async fn classify(pool: &InferenceProviderPool) -> Result<bytes::Bytes, PrivacyClassifyError> {
    pool.privacy_classify(
        MODEL,
        bytes::Bytes::from_static(b"{}"),
        std::collections::HashMap::new(),
    )
    .await
}

fn overloaded() -> Result<bytes::Bytes, PrivacyClassifyError> {
    Err(PrivacyClassifyError::Overloaded {
        retry_after_secs: Some(2),
    })
}

fn timed_out() -> Result<bytes::Bytes, PrivacyClassifyError> {
    Err(PrivacyClassifyError::HttpError {
        status_code: 504,
        message: "upstream text".to_string(),
    })
}

fn unreachable() -> Result<bytes::Bytes, PrivacyClassifyError> {
    Err(PrivacyClassifyError::RequestFailed(
        "connection refused".to_string(),
    ))
}

fn rejected() -> Result<bytes::Bytes, PrivacyClassifyError> {
    Err(PrivacyClassifyError::HttpError {
        status_code: 422,
        message: "upstream text".to_string(),
    })
}

fn ok() -> Result<bytes::Bytes, PrivacyClassifyError> {
    Ok(bytes::Bytes::from_static(b"{\"data\":[]}"))
}

#[tokio::test]
async fn overload_from_the_only_provider_is_returned_without_resending() {
    let (provider, calls) = counting_provider(ProviderTier::Near, overloaded);
    let pool = pool_with(vec![provider]).await;

    let err = classify(&pool).await.unwrap_err();

    assert!(
        matches!(
            err,
            PrivacyClassifyError::Overloaded {
                retry_after_secs: Some(2)
            }
        ),
        "{err:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn overload_falls_through_to_a_different_provider() {
    let (near, near_calls) = counting_provider(ProviderTier::Near, overloaded);
    let (backup, backup_calls) = counting_provider(ProviderTier::Attested3p, ok);
    let pool = pool_with(vec![near, backup]).await;

    assert!(classify(&pool).await.is_ok());
    assert_eq!(near_calls.load(Ordering::SeqCst), 1);
    assert_eq!(backup_calls.load(Ordering::SeqCst), 1);
}

/// A later provider's unrelated failure must not hide the overload, which
/// tells the caller that retrying shortly can succeed.
#[tokio::test]
async fn overload_is_reported_over_a_later_providers_failure() {
    let (near, _) = counting_provider(ProviderTier::Near, overloaded);
    let (backup, backup_calls) = counting_provider(ProviderTier::Attested3p, unreachable);
    let pool = pool_with(vec![near, backup]).await;

    let err = classify(&pool).await.unwrap_err();

    assert!(
        matches!(
            err,
            PrivacyClassifyError::Overloaded {
                retry_after_secs: Some(2)
            }
        ),
        "{err:?}"
    );
    assert_eq!(backup_calls.load(Ordering::SeqCst), 1);
}

/// A client error from another provider means the payload itself is
/// rejected, so it is reported instead of an overload the caller would retry.
#[tokio::test]
async fn client_error_is_reported_over_an_earlier_overload() {
    let (near, _) = counting_provider(ProviderTier::Near, overloaded);
    let (backup, _) = counting_provider(ProviderTier::Attested3p, rejected);
    let pool = pool_with(vec![near, backup]).await;

    let err = classify(&pool).await.unwrap_err();

    assert!(
        matches!(
            err,
            PrivacyClassifyError::HttpError {
                status_code: 422,
                ..
            }
        ),
        "{err:?}"
    );
}

/// A client error is kept even when a later provider fails differently.
#[tokio::test]
async fn client_error_is_not_replaced_by_a_later_failure() {
    let (near, _) = counting_provider(ProviderTier::Near, rejected);
    let (last, last_calls) = counting_provider(ProviderTier::Attested3p, unreachable);
    let pool = pool_with(vec![near, last]).await;

    let err = classify(&pool).await.unwrap_err();

    assert_eq!(
        last_calls.load(Ordering::SeqCst),
        1,
        "the later provider must have been tried"
    );
    assert!(
        matches!(
            err,
            PrivacyClassifyError::HttpError {
                status_code: 422,
                ..
            }
        ),
        "{err:?}"
    );
}

/// The upstream may still be working on a timed-out request, so it is not
/// sent anywhere else. The body is dropped from the returned error.
#[tokio::test]
async fn timeout_is_returned_without_trying_another_provider() {
    let (near, near_calls) = counting_provider(ProviderTier::Near, timed_out);
    let (backup, backup_calls) = counting_provider(ProviderTier::Attested3p, ok);
    let pool = pool_with(vec![near, backup]).await;

    let err = classify(&pool).await.unwrap_err();

    match err {
        PrivacyClassifyError::HttpError {
            status_code,
            message,
        } => {
            assert_eq!(status_code, 504);
            assert_eq!(message, "PII detector returned HTTP 504");
        }
        other => panic!("expected HttpError 504, got {other:?}"),
    }
    assert_eq!(near_calls.load(Ordering::SeqCst), 1);
    assert_eq!(backup_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn connection_failure_still_falls_through_and_is_sanitized() {
    let (near, _) = counting_provider(ProviderTier::Near, unreachable);
    let (backup, backup_calls) = counting_provider(ProviderTier::Attested3p, unreachable);
    let pool = pool_with(vec![near, backup]).await;

    let err = classify(&pool).await.unwrap_err();

    match err {
        PrivacyClassifyError::RequestFailed(message) => {
            assert_eq!(message, "PII detector unreachable")
        }
        other => panic!("expected RequestFailed, got {other:?}"),
    }
    assert_eq!(backup_calls.load(Ordering::SeqCst), 1);
}
