// E2E tests for Repository-level database operations
// These tests directly test repository behavior with the database
mod common;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use database::{OAuthStateRepository, PgApiKeyModelAffinityRepository};
use services::completions::ports::ApiKeyModelAffinityRepository;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration as StdDuration;
use tokio::sync::Barrier;
use uuid::Uuid;

async fn get_test_pool() -> database::pool::DbPool {
    let (_server, _inference_provider_pool, _mock_provider, database) =
        common::setup_test_server_with_pool().await;
    database.pool().clone()
}

struct TestProviderUrlSelector {
    provider_url: String,
    call_count: Arc<AtomicUsize>,
}

#[async_trait]
impl services::completions::ports::ProviderUrlSelector for TestProviderUrlSelector {
    async fn select_provider_url(&self) -> Result<Option<String>, anyhow::Error> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(StdDuration::from_millis(50)).await;
        Ok(Some(self.provider_url.clone()))
    }
}

// ============================================
// OAuth State Repository Tests
// ============================================

#[tokio::test]
async fn test_create_and_get_oauth_state() {
    let pool = get_test_pool().await;
    let repo = OAuthStateRepository::new(pool.clone());

    let state = format!("test-state-{}", uuid::Uuid::new_v4());
    let provider = "github".to_string();

    // Create state
    let created = repo
        .create(state.clone(), provider.clone(), None, None)
        .await
        .unwrap();
    assert_eq!(created.state, state);
    assert_eq!(created.provider, provider);
    assert_eq!(created.pkce_verifier, None);
    assert_eq!(created.frontend_callback, None);

    // Get and delete state
    let retrieved = repo.get_and_delete(&state).await.unwrap();
    assert!(retrieved.is_some());
    let retrieved = retrieved.unwrap();
    assert_eq!(retrieved.state, state);
    assert_eq!(retrieved.provider, provider);

    // Second get should return None (state was deleted)
    let second_get = repo.get_and_delete(&state).await.unwrap();
    assert!(second_get.is_none());
}

#[tokio::test]
async fn test_expired_state_not_returned() {
    let pool = get_test_pool().await;
    let repo = OAuthStateRepository::new(pool.clone());

    let state = format!("test-state-{}", uuid::Uuid::new_v4());

    // Create state with past expiration
    let client = pool.get().await.unwrap();
    let past_time = Utc::now() - Duration::minutes(1);
    client
        .execute(
            r#"
            INSERT INTO oauth_states (state, provider, pkce_verifier, created_at, expires_at)
            VALUES ($1, $2, $3, $4, $5)
            "#,
            &[&state, &"github", &None::<String>, &past_time, &past_time],
        )
        .await
        .unwrap();

    // Try to get expired state
    let result = repo.get_and_delete(&state).await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn test_google_with_pkce_verifier() {
    let pool = get_test_pool().await;
    let repo = OAuthStateRepository::new(pool.clone());

    let state = format!("test-state-{}", uuid::Uuid::new_v4());
    let provider = "google".to_string();
    let verifier = Some("test-pkce-verifier".to_string());

    // Create state with PKCE verifier
    let created = repo
        .create(state.clone(), provider.clone(), verifier.clone(), None)
        .await
        .unwrap();
    assert_eq!(created.pkce_verifier, verifier);
    assert_eq!(created.frontend_callback, None);

    // Get and verify PKCE verifier is preserved
    let retrieved = repo.get_and_delete(&state).await.unwrap().unwrap();
    assert_eq!(retrieved.pkce_verifier, verifier);
}

#[tokio::test]
async fn test_state_replay_protection() {
    let pool = get_test_pool().await;
    let repo = OAuthStateRepository::new(pool.clone());

    let state = format!("test-state-{}", uuid::Uuid::new_v4());
    let provider = "github".to_string();

    // Create one state
    repo.create(state.clone(), provider, None, None)
        .await
        .unwrap();

    // First get should succeed
    let first = repo.get_and_delete(&state).await.unwrap();
    assert!(first.is_some());

    // Second get should fail (replay protection)
    let second = repo.get_and_delete(&state).await.unwrap();
    assert!(second.is_none());
}

// ============================================
// API Key Model Affinity Repository Tests
// ============================================

#[tokio::test]
async fn test_affinity_get_or_create_uses_advisory_lock_for_concurrent_miss() {
    let pool = get_test_pool().await;
    let repo = Arc::new(PgApiKeyModelAffinityRepository::new(pool.clone()));

    let api_key_id = Uuid::new_v4();
    let model_name = format!("test-model-{}", Uuid::new_v4());
    let provider_url = "http://10.0.0.7:8000".to_string();
    let selector_calls = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(4));

    let mut handles: Vec<tokio::task::JoinHandle<anyhow::Result<Option<String>>>> = Vec::new();
    for _ in 0..4 {
        let repo = repo.clone();
        let barrier = barrier.clone();
        let selector_calls = selector_calls.clone();
        let provider_url = provider_url.clone();
        let model_name = model_name.clone();

        handles.push(tokio::spawn(async move {
            let selector = TestProviderUrlSelector {
                provider_url,
                call_count: selector_calls,
            };

            barrier.wait().await;

            repo.get_or_create_active_provider_url(
                api_key_id,
                &model_name,
                StdDuration::from_secs(300),
                &selector,
            )
            .await
        }));
    }

    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.unwrap().unwrap());
    }

    assert_eq!(selector_calls.load(Ordering::SeqCst), 1);
    assert_eq!(results.len(), 4);
    assert!(results
        .iter()
        .all(|value: &Option<String>| value.as_deref() == Some(provider_url.as_str())));

    let stored = repo
        .get_active_provider_url(api_key_id, &model_name)
        .await
        .unwrap();
    assert_eq!(stored.as_deref(), Some(provider_url.as_str()));
}
